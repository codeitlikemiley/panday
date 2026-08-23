//! M14.9 — CodeSandbox T3-remote, against a loopback server that answers like
//! the public control plane plus this crate's guest HTTP seam.
//!
//! No live CodeSandbox calls. No secrets in CI. The stub records Authorization
//! so we can prove the workspace token is sent as Bearer and never lands in a
//! query string.

use futures_util::StreamExt;
use panday_sandbox::t3_remote::{CsbSandbox, CsbToken, CSB_DEFAULT_TEMPLATE};
use panday_sandbox::{
    ExecChunk, ExecSpec, FsPolicy, Limits, NetPolicy, Sandbox, SandboxError, SandboxPolicy,
    SandboxTier, SessionSpec,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    authorization: String,
    body: String,
}

struct Stub {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Stub {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let pitcher = base.clone();

        tokio::spawn(async move {
            let mut files: HashMap<String, Vec<u8>> = HashMap::new();
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                let pitcher = pitcher.clone();
                if let Some(seen) = read_request(&mut stream).await {
                    recorded.lock().unwrap().push(seen.clone());
                    let (status, body, binary) = reply(&seen, &pitcher, &mut files);
                    let _ = write_response(&mut stream, status, &body, binary.as_deref()).await;
                }
            }
        });

        Self { base, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Seen> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(at) = find_header_end(&raw) {
            break Some(at);
        }
        if raw.len() > 64 * 1024 {
            return None;
        }
    }?;
    let header = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut content_length = 0usize;
    let mut authorization = String::new();
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        if let Some(v) = line.split_once(':') {
            if v.0.eq_ignore_ascii_case("authorization") {
                authorization = v.1.trim().to_string();
            }
        }
    }
    let mut body = raw[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_length);
    Some(Seen {
        method,
        path,
        query,
        authorization,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    json: &str,
    binary: Option<&[u8]>,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        404 => "Not Found",
        _ => "Error",
    };
    let (ctype, payload) = if let Some(bin) = binary {
        ("application/octet-stream", bin.to_vec())
    } else {
        ("application/json", json.as_bytes().to_vec())
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await
}

fn reply(
    seen: &Seen,
    pitcher: &str,
    files: &mut HashMap<String, Vec<u8>>,
) -> (u16, String, Option<Vec<u8>>) {
    match (seen.method.as_str(), seen.path.as_str()) {
        ("POST", path) if path == format!("/sandbox/{CSB_DEFAULT_TEMPLATE}/fork") => (
            201,
            r#"{"success":true,"data":{"id":"sbx_mock_1"}}"#.into(),
            None,
        ),
        ("POST", "/vm/sbx_mock_1/start") => (
            200,
            format!(
                r#"{{"success":true,"data":{{"id":"sbx_mock_1","pitcher_url":"{pitcher}","pitcher_token":"ptok_guest","workspace_path":"/project/workspace","bootup_type":"CLEAN","cluster":"test"}}}}"#
            ),
            None,
        ),
        ("POST", "/commands/run") => {
            let argv = command_argv(&seen.body);
            let stdout = if argv.first().map(String::as_str) == Some("echo") {
                argv.get(1).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            (
                200,
                format!(r#"{{"stdout":"{stdout}","stderr":"","exit_code":0}}"#),
                None,
            )
        }
        ("POST", "/vm/sbx_mock_1/hibernate") => (200, r#"{"success":true,"data":{}}"#.into(), None),
        ("DELETE", "/vm/sbx_mock_1") => (200, r#"{"success":true,"data":{}}"#.into(), None),
        ("PUT", "/fs") => {
            let path = query_path(&seen.query);
            files.insert(path, seen.body.as_bytes().to_vec());
            (204, String::new(), None)
        }
        ("GET", "/fs") => {
            let path = query_path(&seen.query);
            match files.get(&path) {
                Some(bytes) => (200, String::new(), Some(bytes.clone())),
                None => (404, r#"{"errors":["not found"]}"#.into(), None),
            }
        }
        _ => (
            404,
            format!(
                r#"{{"errors":["no stub for {} {}"]}}"#,
                seen.method, seen.path
            ),
            None,
        ),
    }
}

fn command_argv(body: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    v.get("argv")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn query_path(query: &str) -> String {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("path="))
        .unwrap_or("")
        .replace("%2F", "/")
        .replace("%2f", "/")
}

fn spec() -> SessionSpec {
    SessionSpec {
        tier: SandboxTier::T3Remote,
        policy: SandboxPolicy {
            fs: FsPolicy::default(),
            net: NetPolicy::default(),
            limits: Limits::default(),
            env: vec![],
        },
    }
}

async fn collect_exec(
    sandbox: &CsbSandbox,
    handle: &panday_sandbox::SandboxHandle,
) -> (String, i32) {
    let mut stream = sandbox
        .exec(
            handle,
            ExecSpec {
                cmd: vec!["echo".into(), "hello-from-csb".into()],
                cwd: None,
                pty: false,
                stdin: None,
            },
        )
        .await
        .unwrap();
    let mut stdout = Vec::new();
    let mut code = -1;
    while let Some(chunk) = stream.next().await {
        match chunk.unwrap() {
            ExecChunk::Stdout(b) => stdout.extend(b),
            ExecChunk::Exit { code: c, .. } => code = c,
            ExecChunk::Stderr(_) => {}
        }
    }
    (String::from_utf8(stdout).unwrap(), code)
}

#[tokio::test]
async fn no_token_is_a_typed_error() {
    // Constructed without reading the process environment, so a developer
    // machine with CSB_API_KEY set cannot accidentally make this green.
    let err = CsbToken::from_secret("").unwrap_err();
    assert!(matches!(err, SandboxError::MissingRemoteToken), "{err:?}");
    let err = CsbSandbox::from_env_or_vault(None);
    // from_env_or_vault(None) still consults CSB_API_KEY; the typed constructor
    // above is the one that must stay red. This arm documents the helper.
    if let Err(e) = err {
        assert!(matches!(e, SandboxError::MissingRemoteToken), "{e:?}");
    }
}

#[tokio::test]
async fn create_exec_echo_destroy() {
    let stub = Stub::start().await;
    let token = CsbToken::from_secret("csb_test_workspace_token").unwrap();
    let sandbox = CsbSandbox::with_base_url(token, &stub.base).unwrap();

    let handle = sandbox.create(spec()).await.expect("create");
    assert_eq!(handle.tier, SandboxTier::T3Remote);
    assert_eq!(handle.id, "sbx_mock_1");

    let (stdout, code) = collect_exec(&sandbox, &handle).await;
    assert_eq!(code, 0);
    assert_eq!(stdout, "hello-from-csb");

    sandbox.destroy(handle).await.expect("destroy");

    let seen = stub.seen();
    let methods: Vec<_> = seen
        .iter()
        .map(|s| format!("{} {}", s.method, s.path))
        .collect();
    assert!(
        methods
            .iter()
            .any(|m| m.contains("POST /sandbox/") && m.contains("/fork")),
        "fork missing: {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m == "POST /vm/sbx_mock_1/start"),
        "start missing: {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m == "POST /commands/run"),
        "exec missing: {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m == "DELETE /vm/sbx_mock_1"),
        "destroy missing: {methods:?}"
    );

    for req in &seen {
        assert!(
            !req.query.contains("csb_test_workspace_token"),
            "token leaked into query: {}",
            req.query
        );
        assert!(
            !req.path.contains("csb_test_workspace_token"),
            "token leaked into path: {}",
            req.path
        );
    }
    let fork = seen
        .iter()
        .find(|s| s.path.contains("/fork"))
        .expect("fork");
    assert_eq!(fork.authorization, "Bearer csb_test_workspace_token");
    let exec = seen
        .iter()
        .find(|s| s.path == "/commands/run")
        .expect("exec");
    assert_eq!(
        exec.authorization, "Bearer ptok_guest",
        "guest I/O must use the pitcher token, not the workspace key"
    );
}

#[tokio::test]
async fn snapshot_is_hibernate_and_put_get_roundtrip() {
    let stub = Stub::start().await;
    let sandbox = CsbSandbox::with_base_url(
        CsbToken::from_secret("csb_test_workspace_token").unwrap(),
        &stub.base,
    )
    .unwrap();
    let handle = sandbox.create(spec()).await.unwrap();
    sandbox
        .put(&handle, PathBuf::from("note.txt"), b"hi".to_vec())
        .await
        .unwrap();
    let got = sandbox
        .get(&handle, PathBuf::from("note.txt"))
        .await
        .unwrap();
    assert_eq!(got, b"hi");
    let snap = sandbox.snapshot(&handle).await.unwrap();
    assert_eq!(snap.0, handle.id);
    sandbox.destroy(handle).await.unwrap();
    let seen = stub.seen();
    assert!(seen
        .iter()
        .any(|s| s.method == "POST" && s.path == "/vm/sbx_mock_1/hibernate"));
}

#[tokio::test]
async fn a_wrong_tier_is_unsupported_not_a_remote_call() {
    let stub = Stub::start().await;
    let sandbox = CsbSandbox::with_base_url(
        CsbToken::from_secret("csb_test_workspace_token").unwrap(),
        &stub.base,
    )
    .unwrap();
    let mut spec = spec();
    spec.tier = SandboxTier::T2OsJail;
    let err = sandbox.create(spec).await.unwrap_err();
    assert!(matches!(
        err,
        SandboxError::Unsupported(SandboxTier::T2OsJail)
    ));
    assert!(
        stub.seen().is_empty(),
        "must not call CSB for the wrong tier"
    );
}
