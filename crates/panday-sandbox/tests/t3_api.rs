//! M14.5 — the Firecracker client, against a socket that answers like Firecracker.
//!
//! What this can establish without KVM: that we form the requests Firecracker documents, in the
//! order it requires, and read its answers — including its errors — correctly. That is precisely
//! where a hand-rolled client goes wrong, and it is the reason docs/14 chose to own one.
//!
//! What it cannot establish is that a microVM boots, or how fast. Those need hardware, and the
//! milestone says so rather than substituting a number.

use panday_sandbox::t3::api::{
    Action, ApiError, BootSource, Drive, FirecrackerApi, LoadSnapshot, MachineConfig, MemBackend,
    NetworkInterface,
};
use panday_sandbox::t3::{Images, T3Sandbox};
use panday_sandbox::{Sandbox, SandboxPolicy, SandboxTier, SessionSpec};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A name nothing else will take. No uuid dependency in this crate, and a counter plus the clock is
/// enough for a directory name in a test.
fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    // Short, because it goes into a socket path with a hard length limit.
    format!(
        "{:x}{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// One request as the stub saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    method: String,
    path: String,
    body: String,
}

/// A Unix socket that answers like Firecracker and records what it was asked.
struct Stub {
    path: PathBuf,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Stub {
    /// `reply` decides the response for each request, so a test can make one call fail.
    async fn start(reply: fn(&Seen) -> String) -> Stub {
        // Short path on purpose: a Unix socket path has a hard length limit (~104 bytes on macOS),
        // and the usual temp directory plus a unique suffix blows straight past it — which
        // presents as `path must be shorter than SUN_LEN` rather than as anything about sockets.
        let path = PathBuf::from(format!("/tmp/pfc{}.sock", unique()));
        let _ = std::fs::remove_file(&path);

        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    // Framed by Content-Length, exactly as Firecracker does — a stub that waited
                    // for EOF would let a client that never half-closes pass here and deadlock
                    // against the real VMM.
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 1024];
                    let text = loop {
                        let Ok(read) = stream.read(&mut buf).await else {
                            return;
                        };
                        if read == 0 {
                            break String::from_utf8_lossy(&raw).to_string();
                        }
                        raw.extend_from_slice(&buf[..read]);
                        let text = String::from_utf8_lossy(&raw).to_string();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let want: usize = head
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("Content-Length: ")?.trim().parse().ok()
                                })
                                .unwrap_or(0);
                            if body.len() >= want {
                                break text;
                            }
                        }
                    };
                    let mut lines = text.lines();
                    let start = lines.next().unwrap_or_default().to_string();
                    let mut parts = start.split_whitespace();
                    let request = Seen {
                        method: parts.next().unwrap_or_default().to_string(),
                        path: parts.next().unwrap_or_default().to_string(),
                        body: text
                            .split_once("\r\n\r\n")
                            .map(|(_, b)| b)
                            .unwrap_or("")
                            .to_string(),
                    };
                    recorded.lock().unwrap().push(request.clone());
                    let _ = stream.write_all(reply(&request).as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        Stub { path, seen }
    }

    fn client(&self) -> FirecrackerApi {
        FirecrackerApi::new(&self.path)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn ok(_: &Seen) -> String {
    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".into()
}

#[tokio::test]
async fn the_boot_sequence_configures_before_it_starts() {
    // Firecracker rejects configuration once the instance is running, so the order is not a style
    // choice — machine config, boot source and drives all have to precede InstanceStart.
    let stub = Stub::start(ok).await;
    let sandbox = T3Sandbox::new(
        Images {
            kernel: PathBuf::from("/opt/vmlinux"),
            rootfs: PathBuf::from("/srv/golden.ext4"),
        },
        1000,
        1000,
    );

    sandbox
        .boot(&stub.client(), std::path::Path::new("/srv/vm-1.ext4"))
        .await
        .expect("boot sequence");

    let paths: Vec<String> = stub.seen().iter().map(|s| s.path.clone()).collect();
    assert_eq!(
        paths,
        [
            "/machine-config",
            "/boot-source",
            "/drives/rootfs",
            "/actions"
        ]
    );

    let start = stub.seen().last().unwrap().clone();
    assert_eq!(start.method, "PUT");
    assert!(start.body.contains("InstanceStart"), "{}", start.body);
}

#[tokio::test]
async fn each_call_sends_the_json_firecracker_documents() {
    let stub = Stub::start(ok).await;
    let client = stub.client();

    client
        .set_machine_config(&MachineConfig {
            vcpu_count: 4,
            mem_size_mib: 2048,
            smt: false,
            track_dirty_pages: true,
        })
        .await
        .unwrap();
    client
        .set_boot_source(&BootSource::minimal(std::path::Path::new("/opt/vmlinux")))
        .await
        .unwrap();
    client
        .set_network_interface(&NetworkInterface {
            iface_id: "eth0".into(),
            host_dev_name: "tap0".into(),
            guest_mac: Some("AA:BB:CC:DD:EE:01".into()),
        })
        .await
        .unwrap();

    let seen = stub.seen();
    let machine: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(machine["vcpu_count"], 4);
    assert_eq!(machine["smt"], false);

    let boot: serde_json::Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(boot["kernel_image_path"], "/opt/vmlinux");
    assert!(boot["boot_args"].as_str().unwrap().contains("panic=-1"));

    // The NIC id is in the *path*, which is the part a hand-rolled client gets wrong.
    assert_eq!(seen[2].path, "/network-interfaces/eth0");
}

#[tokio::test]
async fn a_snapshot_pauses_first_and_stays_paused() {
    // A snapshot taken from a running VM and then resumed produces two futures of one machine; if
    // both ever run they share identity — same entropy, same connections, same clock.
    let stub = Stub::start(ok).await;
    let sandbox = T3Sandbox::new(
        Images {
            kernel: PathBuf::from("/opt/vmlinux"),
            rootfs: PathBuf::from("/srv/golden.ext4"),
        },
        1000,
        1000,
    );

    sandbox
        .snapshot_paused(
            &stub.client(),
            std::path::Path::new("/snap/vm.snap"),
            std::path::Path::new("/snap/vm.mem"),
        )
        .await
        .unwrap();

    let seen = stub.seen();
    assert_eq!(seen[0].path, "/vm");
    assert_eq!(seen[0].method, "PATCH");
    assert!(seen[0].body.contains("Paused"));
    assert_eq!(seen[1].path, "/snapshot/create");
    // Nothing resumes it afterwards — the caller chooses which copy continues.
    assert_eq!(seen.len(), 2);
}

#[tokio::test]
async fn loading_a_snapshot_can_hold_the_vm_before_it_runs() {
    // What the warm pool needs (M14.6): restore, hold, hand out, resume.
    let stub = Stub::start(ok).await;
    stub.client()
        .load_snapshot(&LoadSnapshot {
            snapshot_path: "/snap/vm.snap".into(),
            mem_backend: MemBackend {
                backend_path: "/snap/vm.mem".into(),
                backend_type: "File".into(),
            },
            resume_vm: false,
        })
        .await
        .unwrap();

    let body: serde_json::Value = serde_json::from_str(&stub.seen()[0].body).unwrap();
    assert_eq!(body["resume_vm"], false);
    assert_eq!(body["mem_backend"]["backend_type"], "File");
}

#[tokio::test]
async fn a_fault_message_reaches_the_caller() {
    fn refuse(_: &Seen) -> String {
        let body = "{\"fault_message\":\"Invalid drive path: No such file or directory\"}";
        format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }
    let stub = Stub::start(refuse).await;

    let err = stub
        .client()
        .set_drive(&Drive {
            drive_id: "rootfs".into(),
            path_on_host: "/nope.ext4".into(),
            is_root_device: true,
            is_read_only: false,
        })
        .await
        .unwrap_err();

    match err {
        ApiError::Status { status, body } => {
            assert_eq!(status, 400);
            assert!(body.contains("No such file"), "{body}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_missing_socket_says_so_rather_than_hanging() {
    let client = FirecrackerApi::new("/definitely/not/a/socket");
    let err = client.action(Action::InstanceStart).await.unwrap_err();
    assert!(matches!(err, ApiError::Connect { .. }), "{err:?}");
}

#[tokio::test]
async fn a_vmm_that_never_answers_times_out() {
    // A wedged VMM must not hang a session actor. The socket accepts and says nothing.
    let path = PathBuf::from(format!("/tmp/pfcm{}.sock", unique()));
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            // Held open, deliberately silent.
            held.push(stream);
        }
    });

    let client = FirecrackerApi::new(&path).with_timeout(std::time::Duration::from_millis(300));
    let err = client.instance_info().await.unwrap_err();
    assert!(matches!(err, ApiError::Timeout(_)), "{err:?}");
}

#[tokio::test]
async fn instance_info_is_parsed() {
    fn info(_: &Seen) -> String {
        let body = "{\"id\":\"vm-1\",\"state\":\"Running\",\"vmm_version\":\"1.7.0\"}";
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }
    let stub = Stub::start(info).await;
    let got = stub.client().instance_info().await.unwrap();
    assert_eq!(got.state, "Running");
    assert_eq!(got.vmm_version, "1.7.0");
}

#[tokio::test]
async fn without_kvm_the_tier_refuses_by_name_instead_of_degrading() {
    // The worst possible failure for this tier would be quietly becoming a weaker one: T3 exists
    // because T2 is not enough for a stranger's code.
    if panday_sandbox::t3::kvm_available() {
        return; // On a KVM host this says nothing; the pool manager's suite covers that path.
    }
    let sandbox = T3Sandbox::new(
        Images {
            kernel: PathBuf::from("/opt/vmlinux"),
            rootfs: PathBuf::from("/srv/golden.ext4"),
        },
        1000,
        1000,
    );
    let err = sandbox
        .create(SessionSpec {
            tier: SandboxTier::T3MicroVm,
            policy: SandboxPolicy::default(),
        })
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(err.to_string().contains("/dev/kvm"), "{err}");
}

#[test]
fn the_factory_and_the_jailer_agree_about_where_the_socket_is() {
    // The two halves of the tier meet here: the jailer decides where the socket appears, so a
    // caller that reconstructed the path independently would eventually disagree with it.
    let sandbox = T3Sandbox::new(
        Images {
            kernel: PathBuf::from("/opt/vmlinux"),
            rootfs: PathBuf::from("/srv/golden.ext4"),
        },
        1000,
        1000,
    );
    let jail = sandbox.jail("vm-42");
    assert!(jail
        .api_socket()
        .ends_with("firecracker/vm-42/root/run/firecracker.socket"));
    // And it carries the unprivileged ids the factory was built with.
    assert!(jail.args().is_ok());
}

// ── M14.6: the pool's production backend ──────────────────────────────────────

#[tokio::test]
async fn the_pool_backend_refuses_without_kvm_rather_than_failing_obscurely() {
    use panday_sandbox::t3::{SnapshotPaths, VmBackend};

    if panday_sandbox::t3::kvm_available() {
        return;
    }
    let sandbox = T3Sandbox::new(
        Images {
            kernel: PathBuf::from("/opt/vmlinux"),
            rootfs: PathBuf::from("/srv/golden.ext4"),
        },
        1000,
        1000,
    );
    let backend = sandbox.pool_backend(SnapshotPaths {
        snapshot: PathBuf::from("/snap/golden.snap"),
        memory: PathBuf::from("/snap/golden.mem"),
        chroot_base: PathBuf::from("/srv/jailer"),
    });

    let err = backend.restore().await.map(|_| ()).unwrap_err();
    assert!(err.to_string().contains("/dev/kvm"), "{err}");
}

#[test]
fn the_backend_looks_for_each_vms_socket_under_its_own_jail() {
    // Two VMs in one pool must not share a socket path, or the second one's API calls land in the
    // first one's VMM.
    use panday_sandbox::t3::SnapshotPaths;

    let sandbox = T3Sandbox::new(
        Images {
            kernel: PathBuf::from("/opt/vmlinux"),
            rootfs: PathBuf::from("/srv/golden.ext4"),
        },
        1000,
        1000,
    );
    let backend = sandbox.pool_backend(SnapshotPaths {
        snapshot: PathBuf::from("/snap/golden.snap"),
        memory: PathBuf::from("/snap/golden.mem"),
        chroot_base: PathBuf::from("/srv/jailer"),
    });

    let a = backend.client_for("warm-1");
    let b = backend.client_for("warm-2");
    assert_ne!(a.socket(), b.socket());
    assert!(a.socket().starts_with("/srv/jailer"));
}
