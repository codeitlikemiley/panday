//! Firecracker's REST-over-UDS API (docs/14 M14.5).
//!
//! > "T3 drives Firecracker's REST-over-UDS API directly. The community Rust SDK situation is thin
//! > (fctools 'semi-stable', others stale) — the API is small and stable; we own a 500-line client."
//!
//! This is that client. It speaks HTTP/1.1 over a Unix socket by hand, which sounds worse than it
//! is: the API has six endpoints, no chunked encoding, no keep-alive negotiation, and JSON bodies
//! whose shapes are pinned by Firecracker's own OpenAPI spec. What a general HTTP stack would add
//! here is a connector, a runtime integration, and a dependency that has to be right about
//! everything — for six requests that always look the same.
//!
//! **Every call is one connection.** Firecracker's socket handles that happily, and it removes the
//! entire class of bug where a half-read response from a previous request desynchronises the next
//! one — which is exactly the bug a hand-rolled client would otherwise have.
//!
//! Testable without KVM: the client is pointed at any Unix socket, so the suite runs it against a
//! stub that speaks the same protocol. What that cannot prove is that a microVM boots; what it does
//! prove is that we form the requests Firecracker documents and read its answers correctly, which
//! is where a hand-rolled client actually goes wrong.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("connect {socket}: {detail}")]
    Connect { socket: String, detail: String },
    #[error("io: {0}")]
    Io(String),
    #[error("firecracker said {status}: {body}")]
    Status { status: u16, body: String },
    #[error("malformed response: {0}")]
    Malformed(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

/// A Firecracker API socket.
#[derive(Debug, Clone)]
pub struct FirecrackerApi {
    socket: PathBuf,
    /// Every request is bounded. A VMM that stops answering must not hang a session actor.
    timeout: Duration,
}

/// Where the kernel and its command line come from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootSource {
    pub kernel_image_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_args: Option<String>,
}

impl BootSource {
    /// The boot args a sandbox wants: no console spew, no init system, straight to our agent.
    ///
    /// `panic=-1` matters — a guest kernel panic should reboot into nothing and let the host notice
    /// a dead VM, rather than sitting at a panic prompt holding a slot in the pool forever.
    pub fn minimal(kernel: &Path) -> Self {
        Self {
            kernel_image_path: kernel.display().to_string(),
            boot_args: Some(
                "console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux i8042.nomux \
                 i8042.nopnp i8042.dumbkbd"
                    .into(),
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    /// Off. Hyperthread siblings share a core, and two sandboxes that share a core share a timing
    /// side channel — which is the entire reason a stranger's code is in a VM rather than a jail.
    pub smt: bool,
    /// Required for snapshot/restore across hosts (M14.6): without it a guest may use instructions
    /// the restoring host does not have, and the restore fails at an unpredictable later moment.
    pub track_dirty_pages: bool,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            vcpu_count: 2,
            mem_size_mib: 1024,
            smt: false,
            track_dirty_pages: true,
        }
    }
}

/// A host-side tap device the guest sees as a NIC.
///
/// Present so the egress proxy (docs/14 §network) has something to attach to. A VM with no network
/// interface at all cannot be given one later without a reboot, and the default-deny happens at the
/// proxy rather than by leaving the guest deaf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub host_dev_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_mac: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum Action {
    InstanceStart,
    FlushMetrics,
    SendCtrlAltDel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum VmState {
    Paused,
    Resumed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum SnapshotType {
    Full,
    Diff,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateSnapshot {
    pub snapshot_path: String,
    pub mem_file_path: String,
    pub snapshot_type: SnapshotType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadSnapshot {
    pub snapshot_path: String,
    pub mem_backend: MemBackend,
    /// Resume immediately. `false` leaves the VM paused so a caller can attach before it runs —
    /// which is what the warm pool does (M14.6): restore, hold, hand out, resume.
    pub resume_vm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemBackend {
    pub backend_path: String,
    pub backend_type: String,
}

/// What Firecracker reports about itself.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct InstanceInfo {
    pub id: String,
    pub state: String,
    pub vmm_version: String,
}

impl FirecrackerApi {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            // Generous for a boot, tight enough that a wedged VMM is noticed within a turn.
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub async fn instance_info(&self) -> Result<InstanceInfo, ApiError> {
        let body = self.request("GET", "/", None::<&()>).await?;
        serde_json::from_str(&body).map_err(|e| ApiError::Malformed(e.to_string()))
    }

    pub async fn set_boot_source(&self, boot: &BootSource) -> Result<(), ApiError> {
        self.request("PUT", "/boot-source", Some(boot)).await?;
        Ok(())
    }

    pub async fn set_drive(&self, drive: &Drive) -> Result<(), ApiError> {
        let path = format!("/drives/{}", drive.drive_id);
        self.request("PUT", &path, Some(drive)).await?;
        Ok(())
    }

    pub async fn set_machine_config(&self, config: &MachineConfig) -> Result<(), ApiError> {
        self.request("PUT", "/machine-config", Some(config)).await?;
        Ok(())
    }

    pub async fn set_network_interface(&self, nic: &NetworkInterface) -> Result<(), ApiError> {
        let path = format!("/network-interfaces/{}", nic.iface_id);
        self.request("PUT", &path, Some(nic)).await?;
        Ok(())
    }

    pub async fn action(&self, action: Action) -> Result<(), ApiError> {
        #[derive(Serialize)]
        struct Body {
            action_type: Action,
        }
        self.request(
            "PUT",
            "/actions",
            Some(&Body {
                action_type: action,
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn set_vm_state(&self, state: VmState) -> Result<(), ApiError> {
        #[derive(Serialize)]
        struct Body {
            state: VmState,
        }
        self.request("PATCH", "/vm", Some(&Body { state })).await?;
        Ok(())
    }

    pub async fn create_snapshot(&self, snapshot: &CreateSnapshot) -> Result<(), ApiError> {
        self.request("PUT", "/snapshot/create", Some(snapshot))
            .await?;
        Ok(())
    }

    pub async fn load_snapshot(&self, snapshot: &LoadSnapshot) -> Result<(), ApiError> {
        self.request("PUT", "/snapshot/load", Some(snapshot))
            .await?;
        Ok(())
    }

    /// One request, one connection, one response.
    async fn request<T: Serialize>(
        &self,
        method: &str,
        path: &str,
        body: Option<&T>,
    ) -> Result<String, ApiError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|e| ApiError::Connect {
                socket: self.socket.display().to_string(),
                detail: e.to_string(),
            })?;

        let encoded = match body {
            Some(b) => serde_json::to_string(b).map_err(|e| ApiError::Malformed(e.to_string()))?,
            None => String::new(),
        };
        // `Connection: close` is what makes one-connection-per-request honest: the server closes,
        // the read ends at EOF, and there is no framing left to get wrong.
        let request = format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Accept: application/json\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{encoded}",
            encoded.len()
        );

        let exchange = async {
            stream
                .write_all(request.as_bytes())
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?;
            // Half-close: the request is complete, and saying so lets a server that frames by EOF
            // answer without waiting for a timeout. Firecracker frames by Content-Length and does
            // not need this, which is exactly why it is easy to leave out and then deadlock
            // against anything that does.
            stream
                .shutdown()
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?;
            let mut raw = Vec::new();
            stream
                .read_to_end(&mut raw)
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?;
            Ok::<_, ApiError>(raw)
        };

        let raw = tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| ApiError::Timeout(self.timeout))??;

        parse_response(&String::from_utf8_lossy(&raw))
    }
}

/// Split an HTTP/1.1 response into a status and a body, and turn a non-2xx into an error.
///
/// Firecracker's errors carry a `fault_message` that names the field it rejected, and losing it in
/// favour of "500" would turn a typo in a drive path into an afternoon.
pub fn parse_response(text: &str) -> Result<String, ApiError> {
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((head, body)) => (head, body.to_string()),
        None => (text, String::new()),
    };

    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| ApiError::Malformed(format!("no status line in {head:?}")))?;

    if (200..300).contains(&status) {
        return Ok(body);
    }

    // Unwrap the fault message when there is one; keep the raw body when there is not.
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("fault_message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or(body);

    Err(ApiError::Status {
        status,
        body: detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_success_yields_its_body() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_response(response).unwrap(), "{}");
        // 204 is what most of Firecracker's PUTs actually return.
        assert_eq!(
            parse_response("HTTP/1.1 204 No Content\r\n\r\n").unwrap(),
            ""
        );
    }

    #[test]
    fn a_fault_message_survives_instead_of_being_flattened_to_a_number() {
        // A typo in a drive path is an afternoon if all you get back is "400".
        let response = "HTTP/1.1 400 Bad Request\r\n\r\n\
                        {\"fault_message\":\"No such file or directory (os error 2)\"}";
        match parse_response(response) {
            Err(ApiError::Status { status, body }) => {
                assert_eq!(status, 400);
                assert!(body.contains("No such file"), "{body}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_non_json_error_body_is_kept_verbatim() {
        let response = "HTTP/1.1 500 Internal Server Error\r\n\r\nsomething went very wrong";
        match parse_response(response) {
            Err(ApiError::Status { body, .. }) => assert_eq!(body, "something went very wrong"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_response_without_a_status_line_is_an_error_not_a_panic() {
        assert!(matches!(
            parse_response("garbage"),
            Err(ApiError::Malformed(_))
        ));
        assert!(matches!(parse_response(""), Err(ApiError::Malformed(_))));
    }

    #[test]
    fn the_boot_args_turn_off_what_a_sandbox_does_not_need() {
        let boot = BootSource::minimal(Path::new("/opt/vmlinux"));
        let args = boot.boot_args.unwrap();
        // A guest kernel panic should end the VM, not hold a pool slot at a prompt forever.
        assert!(args.contains("panic=-1"), "{args}");
        assert!(args.contains("reboot=k"), "{args}");
        assert!(args.contains("pci=off"), "{args}");
    }

    #[test]
    fn the_machine_config_disables_hyperthread_siblings() {
        // Two sandboxes sharing a core share a timing side channel, which is the entire reason a
        // stranger's code is in a VM rather than a jail.
        let config = MachineConfig::default();
        assert!(!config.smt);
        // Dirty-page tracking is what makes cross-host snapshot restore possible at all (M14.6).
        assert!(config.track_dirty_pages);
    }

    #[test]
    fn the_wire_shapes_match_firecrackers_own_spelling() {
        // PascalCase actions and states, snake_case fields: Firecracker's OpenAPI spec, and the
        // thing a hand-rolled client is most likely to get subtly wrong.
        assert_eq!(
            serde_json::to_string(&Action::InstanceStart).unwrap(),
            "\"InstanceStart\""
        );
        assert_eq!(
            serde_json::to_string(&VmState::Paused).unwrap(),
            "\"Paused\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotType::Full).unwrap(),
            "\"Full\""
        );

        let drive = Drive {
            drive_id: "rootfs".into(),
            path_on_host: "/srv/rootfs.ext4".into(),
            is_root_device: true,
            is_read_only: false,
        };
        let json = serde_json::to_value(&drive).unwrap();
        assert_eq!(json["is_root_device"], true);
        assert_eq!(json["path_on_host"], "/srv/rootfs.ext4");

        let config = serde_json::to_value(MachineConfig::default()).unwrap();
        assert_eq!(config["vcpu_count"], 2);
        assert_eq!(config["mem_size_mib"], 1024);
        assert_eq!(config["track_dirty_pages"], true);
    }
}
