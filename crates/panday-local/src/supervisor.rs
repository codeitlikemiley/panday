//! The model-runner supervisor (docs/18, M18.2).
//!
//! > "spawns and health-checks a local inference server; v1 target **llama-server** (llama.cpp) as
//! > default, **mistral.rs** as the Rust-native alternative (both OpenAI-compat, both GGUF; the
//! > gateway's `local` adapter doesn't care which)"
//!
//! Which is why this supervises a *command* rather than knowing anything about llama.cpp: the
//! contract is "a process that serves an OpenAI-compatible API on a port and answers a health
//! check". A supervisor that parsed llama-server's flags would have to be rewritten for mistral.rs,
//! and the adapter that talks to both already proves it does not need to be.
//!
//! **Restarts are bounded.** A model that crashes on load crashes on every load; restarting it
//! forever burns a laptop's battery and buries the one error message that explains why. After
//! `max_restarts` inside `window` the supervisor stops and holds the last exit status, which is
//! what a caller needs to print.
//!
//! **Health is a trait.** `HttpHealth` is what production uses; a test needs to check the
//! *supervision* — does it notice a death, does it back off, does it give up — without standing up
//! an inference server to do it.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("spawn {binary}: {detail}")]
    Spawn { binary: String, detail: String },
    #[error("`{binary}` did not become healthy within {seconds}s")]
    Unhealthy { binary: String, seconds: u64 },
    #[error("the model server exited during startup: {0}")]
    ExitedEarly(String),
}

/// Whether the thing we spawned is ready to serve.
#[async_trait::async_trait]
pub trait Health: Send + Sync {
    async fn ready(&self) -> bool;
}

/// An HTTP GET that has to return 2xx. What llama-server and mistral.rs both offer.
pub struct HttpHealth {
    pub url: String,
    pub timeout: Duration,
}

impl HttpHealth {
    pub fn new(base_url: &str) -> Self {
        Self {
            // `/health` on llama-server; `/v1/models` also works and is the mistral.rs fallback.
            url: format!("{}/health", base_url.trim_end_matches('/')),
            timeout: Duration::from_secs(2),
        }
    }
}

#[async_trait::async_trait]
impl Health for HttpHealth {
    async fn ready(&self) -> bool {
        let Ok(client) = reqwest::Client::builder().timeout(self.timeout).build() else {
            return false;
        };
        matches!(client.get(&self.url).send().await, Ok(r) if r.status().is_success())
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// `llama-server`, `mistralrs-server`, or anything else that serves the dialect.
    pub binary: String,
    pub args: Vec<String>,
    /// Where the adapter will point. The supervisor does not choose it: the caller has to know the
    /// URL to configure the gateway, so inventing one here would mean parsing it back out.
    pub base_url: String,
    pub startup_timeout: Duration,
    pub max_restarts: u32,
    pub window: Duration,
}

impl ServerConfig {
    /// llama-server's own flags, which are the defaults docs/18 names.
    pub fn llama_server(model_path: &std::path::Path, port: u16) -> Self {
        Self {
            binary: "llama-server".into(),
            args: vec![
                "-m".into(),
                model_path.display().to_string(),
                "--port".into(),
                port.to_string(),
                // Loopback only. The offline tier's promise is that nothing leaves the machine, and
                // a server bound to 0.0.0.0 breaks it silently on a café network (ADR-011).
                "--host".into(),
                "127.0.0.1".into(),
            ],
            base_url: format!("http://127.0.0.1:{port}"),
            startup_timeout: Duration::from_secs(120),
            max_restarts: 3,
            window: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Starting,
    Healthy,
    Restarting {
        attempts: u32,
    },
    /// Stopped trying. Holds the reason a human needs.
    Failed {
        detail: String,
    },
    Stopped,
}

/// A running (or once-running) model server.
#[derive(Debug)]
pub struct Supervisor {
    config: ServerConfig,
    state: Arc<Mutex<State>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Supervisor {
    /// Spawn, wait for health, and keep it alive until `stop`.
    pub async fn start(
        config: ServerConfig,
        health: Arc<dyn Health>,
    ) -> Result<Self, SupervisorError> {
        let state = Arc::new(Mutex::new(State::Starting));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut child = spawn(&config)?;
        wait_healthy(&config, health.as_ref(), &mut child).await?;
        *state.lock().unwrap() = State::Healthy;

        // The supervision loop. Detached, holding only what it needs, so the caller can drop the
        // handle and still have a server — and can stop it by flipping one flag.
        let loop_state = state.clone();
        let loop_shutdown = shutdown.clone();
        let loop_config = config.clone();
        tokio::spawn(async move {
            supervise(child, loop_config, health, loop_state, loop_shutdown).await;
        });

        Ok(Self {
            config,
            state,
            shutdown,
        })
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    /// Stop supervising and let the process go.
    ///
    /// Sets the flag the loop checks *before* killing, so the death is not read as a crash and
    /// restarted — a supervisor that fights its own shutdown is a process nobody can stop.
    pub fn stop(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *self.state.lock().unwrap() = State::Stopped;
    }
}

fn spawn(config: &ServerConfig) -> Result<tokio::process::Child, SupervisorError> {
    tokio::process::Command::new(&config.binary)
        .args(&config.args)
        // Inherited output. A model server's startup log is the only explanation a user gets for
        // "it did not come up", and swallowing it to keep the terminal tidy is how that becomes
        // unanswerable.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| SupervisorError::Spawn {
            binary: config.binary.clone(),
            detail: e.to_string(),
        })
}

async fn wait_healthy(
    config: &ServerConfig,
    health: &dyn Health,
    child: &mut tokio::process::Child,
) -> Result<(), SupervisorError> {
    let deadline = Instant::now() + config.startup_timeout;
    while Instant::now() < deadline {
        // A process that has already exited will never become healthy; waiting out the full
        // timeout to say so wastes two minutes of somebody's attention.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(SupervisorError::ExitedEarly(status.to_string()));
        }
        if health.ready().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = child.kill().await;
    Err(SupervisorError::Unhealthy {
        binary: config.binary.clone(),
        seconds: config.startup_timeout.as_secs(),
    })
}

async fn supervise(
    mut child: tokio::process::Child,
    config: ServerConfig,
    health: Arc<dyn Health>,
    state: Arc<Mutex<State>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut restarts: Vec<Instant> = Vec::new();

    loop {
        let exit = child.wait().await;
        if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let detail = match exit {
            Ok(status) => status.to_string(),
            Err(e) => e.to_string(),
        };

        // Only the deaths inside the window count. A server that ran fine for a day and then
        // crashed twice is not the same thing as one that never started.
        let now = Instant::now();
        restarts.retain(|at| now.duration_since(*at) < config.window);
        restarts.push(now);
        if restarts.len() as u32 > config.max_restarts {
            *state.lock().unwrap() = State::Failed {
                detail: format!(
                    "{} restarts in {}s; last exit: {detail}",
                    restarts.len(),
                    config.window.as_secs()
                ),
            };
            return;
        }

        let attempts = restarts.len() as u32;
        *state.lock().unwrap() = State::Restarting { attempts };
        tracing::warn!(binary = %config.binary, attempts, %detail, "model server died; restarting");

        // Linear backoff, capped by the window. Exponential would put a laptop to sleep for
        // minutes on the third crash, and the interesting failures happen in the first few seconds.
        tokio::time::sleep(Duration::from_millis(250 * attempts as u64)).await;

        match spawn(&config) {
            Ok(mut next) => {
                if let Err(e) = wait_healthy(&config, health.as_ref(), &mut next).await {
                    *state.lock().unwrap() = State::Failed {
                        detail: e.to_string(),
                    };
                    return;
                }
                *state.lock().unwrap() = State::Healthy;
                child = next;
            }
            Err(e) => {
                *state.lock().unwrap() = State::Failed {
                    detail: e.to_string(),
                };
                return;
            }
        }
    }
}
