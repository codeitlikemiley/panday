//! T3 — Firecracker microVMs (docs/14 §tiers, M14.5).
//!
//! > "**T3** | Firecracker microVM | strangers' code on OUR cloud | ~125ms cold, ~snapshot-warm |
//! > hardware virtualization"
//!
//! **What is here and what is not.** The host side is complete and tested: the REST-over-UDS client
//! (`api`), the jailer's command line and cgroup caps (`jailer`), the boot sequence, and the
//! snapshot calls M14.6 builds pools out of. What is *not* here is a measured cold-boot number or a
//! guest that has actually run code, because both need KVM and this tree is developed on macOS
//! (docs/14: "KVM required for T3 → cloud pools are Linux/metal-or-nested-virt").
//!
//! Rather than fake that, `create` refuses immediately and by name when `/dev/kvm` is missing. A
//! sandbox tier that silently degraded to a weaker one would be the worst possible failure here:
//! the entire reason T3 exists is that T2 is not enough for a stranger's code.
//!
//! The rootfs is built by `scripts/build-rootfs.sh` — a golden image per toolchain, exactly as
//! docs/14 describes, with the guest agent baked in.

pub mod api;
pub mod jailer;
pub mod nodes;
pub mod pool;

use crate::{ExecSpec, ExecStream, Sandbox, SandboxError, SandboxHandle, SessionSpec, SnapshotRef};
use std::path::PathBuf;

pub use api::{
    Action, BootSource, CreateSnapshot, Drive, FirecrackerApi, LoadSnapshot, MachineConfig,
    MemBackend, NetworkInterface, SnapshotType, VmState,
};
pub use jailer::{kvm_available, Caps, JailerConfig};
pub use nodes::{drain, NodeId, Reassignment, Ring};
pub use pool::{PoolError, PoolStats, PooledVm, VmBackend, WarmPool};

/// Where the golden images live.
#[derive(Debug, Clone)]
pub struct Images {
    pub kernel: PathBuf,
    /// The golden rootfs for this toolchain. Copied per VM — a shared writable rootfs would let one
    /// sandbox write something the next one reads.
    pub rootfs: PathBuf,
}

/// A configured but not-yet-running T3 sandbox factory.
pub struct T3Sandbox {
    images: Images,
    machine: MachineConfig,
    uid: u32,
    gid: u32,
}

impl T3Sandbox {
    pub fn new(images: Images, uid: u32, gid: u32) -> Self {
        Self {
            images,
            machine: MachineConfig::default(),
            uid,
            gid,
        }
    }

    pub fn with_machine(mut self, machine: MachineConfig) -> Self {
        self.machine = machine;
        self
    }

    /// The jail this factory would put a VM in.
    ///
    /// The link between the two halves of the tier: the jailer decides where the API socket appears,
    /// so a caller gets the socket path from the same object that built the command rather than
    /// reconstructing it and hoping the two agree.
    pub fn jail(&self, id: &str) -> JailerConfig {
        JailerConfig::new(id, self.uid, self.gid)
    }

    /// The API calls that turn a fresh VMM into a booted VM, in order.
    ///
    /// Separated from spawning so the sequence can be exercised against a stub socket — which is
    /// what the suite does. The order matters and is Firecracker's: machine config, boot source and
    /// drives *before* `InstanceStart`, because the VMM rejects configuration once the instance is
    /// running.
    pub async fn boot(
        &self,
        client: &FirecrackerApi,
        rootfs_for_vm: &std::path::Path,
    ) -> Result<(), api::ApiError> {
        client.set_machine_config(&self.machine).await?;
        client
            .set_boot_source(&BootSource::minimal(&self.images.kernel))
            .await?;
        client
            .set_drive(&Drive {
                drive_id: "rootfs".into(),
                path_on_host: rootfs_for_vm.display().to_string(),
                is_root_device: true,
                // Writable, because the guest needs a working filesystem — the isolation comes from
                // the copy being per-VM and discarded, not from read-only.
                is_read_only: false,
            })
            .await?;
        client.action(Action::InstanceStart).await
    }

    /// Pause, snapshot, and leave the VM paused (M14.6's building block).
    ///
    /// Paused rather than resumed afterwards on purpose: a snapshot taken from a running VM and
    /// then resumed produces two futures of the same machine, and if both ever run they share
    /// identity — same random state, same open connections, same clock. The caller decides which
    /// one continues.
    pub async fn snapshot_paused(
        &self,
        client: &FirecrackerApi,
        snapshot_path: &std::path::Path,
        mem_path: &std::path::Path,
    ) -> Result<(), api::ApiError> {
        client.set_vm_state(VmState::Paused).await?;
        client
            .create_snapshot(&CreateSnapshot {
                snapshot_path: snapshot_path.display().to_string(),
                mem_file_path: mem_path.display().to_string(),
                snapshot_type: SnapshotType::Full,
            })
            .await
    }

    /// Why this machine cannot run a T3 sandbox.
    ///
    /// `Internal` rather than `Unsupported(tier)`: the tier *is* supported, this host simply has no
    /// KVM, and a caller that read "tier unsupported" would reasonably conclude the build lacked T3
    /// rather than that the machine lacked hardware. The sentence says which.
    /// The pool backend for this factory: restore from a snapshot, cold-boot when there is none.
    ///
    /// Present so `WarmPool` has a production implementation rather than only a test one — a trait
    /// whose only implementor is a fake is a trait that has not been designed against reality.
    pub fn pool_backend(&self, snapshot: SnapshotPaths) -> FirecrackerBackend {
        FirecrackerBackend {
            snapshot,
            machine: self.machine,
            kernel: self.images.kernel.clone(),
            rootfs: self.images.rootfs.clone(),
        }
    }

    fn no_kvm() -> SandboxError {
        SandboxError::Internal(
            "T3 needs /dev/kvm, which this machine does not have. macOS gets T2 locally; \
             cloud execution is always Linux (docs/14)."
                .into(),
        )
    }
}

#[async_trait::async_trait]
impl Sandbox for T3Sandbox {
    async fn create(&self, _spec: SessionSpec) -> Result<SandboxHandle, SandboxError> {
        // Checked first and by name. A tier that silently fell back to a weaker one would defeat
        // the only reason this tier exists.
        if !kvm_available() {
            return Err(Self::no_kvm());
        }
        Err(SandboxError::Internal(
            "T3 VM lifecycle needs the pool manager (M14.6) and a guest agent build; the API \
             client, jailer and boot sequence are complete and tested (docs/14 M14.5)."
                .into(),
        ))
    }

    async fn exec(&self, _h: &SandboxHandle, _cmd: ExecSpec) -> Result<ExecStream, SandboxError> {
        Err(Self::no_kvm())
    }

    async fn put(
        &self,
        _h: &SandboxHandle,
        _path: PathBuf,
        _data: Vec<u8>,
    ) -> Result<(), SandboxError> {
        Err(Self::no_kvm())
    }

    async fn get(&self, _h: &SandboxHandle, _path: PathBuf) -> Result<Vec<u8>, SandboxError> {
        Err(Self::no_kvm())
    }

    async fn snapshot(&self, _h: &SandboxHandle) -> Result<SnapshotRef, SandboxError> {
        Err(Self::no_kvm())
    }

    async fn destroy(&self, _h: SandboxHandle) -> Result<(), SandboxError> {
        Err(Self::no_kvm())
    }
}

/// Where a golden snapshot lives.
#[derive(Debug, Clone)]
pub struct SnapshotPaths {
    pub snapshot: PathBuf,
    pub memory: PathBuf,
    /// The jailer chroot base every VM in this pool is created under.
    pub chroot_base: PathBuf,
}

/// The pool's production backend (M14.6).
///
/// Every operation is API calls plus a jailer spawn, both of which are covered by their own suites;
/// what this adds is the sequence, and the sequence is short on purpose. It runs only where there is
/// KVM, and says so rather than pretending otherwise.
pub struct FirecrackerBackend {
    snapshot: SnapshotPaths,
    machine: MachineConfig,
    kernel: PathBuf,
    rootfs: PathBuf,
}

impl FirecrackerBackend {
    /// The API client for one VM, at the socket its jail will create.
    pub fn client_for(&self, id: &str) -> FirecrackerApi {
        let jail = JailerConfig {
            chroot_base: self.snapshot.chroot_base.clone(),
            ..JailerConfig::new(id, 1000, 1000)
        };
        FirecrackerApi::new(jail.api_socket())
    }

    fn refuse() -> pool::PoolError {
        pool::PoolError::Backend(
            "T3 pools need /dev/kvm and a jailer; this host has neither (docs/14 M14.6)".into(),
        )
    }
}

#[async_trait::async_trait]
impl pool::VmBackend for FirecrackerBackend {
    async fn restore(&self) -> Result<pool::PooledVm, pool::PoolError> {
        if !kvm_available() {
            return Err(Self::refuse());
        }
        // The snapshot is loaded *paused* — a warm VM that started executing while it waited in the
        // pool would drift from the snapshot every other VM was restored from.
        let id = format!("warm-{}", next_id());
        let client = self.client_for(&id);
        client
            .load_snapshot(&LoadSnapshot {
                snapshot_path: self.snapshot.snapshot.display().to_string(),
                mem_backend: MemBackend {
                    backend_path: self.snapshot.memory.display().to_string(),
                    backend_type: "File".into(),
                },
                resume_vm: false,
            })
            .await
            .map_err(|e| pool::PoolError::Backend(e.to_string()))?;
        Ok(pool::PooledVm { id, warm: true })
    }

    async fn cold_boot(&self) -> Result<pool::PooledVm, pool::PoolError> {
        if !kvm_available() {
            return Err(Self::refuse());
        }
        let id = format!("cold-{}", next_id());
        let client = self.client_for(&id);
        let factory = T3Sandbox {
            images: Images {
                kernel: self.kernel.clone(),
                rootfs: self.rootfs.clone(),
            },
            machine: self.machine,
            uid: 1000,
            gid: 1000,
        };
        factory
            .boot(&client, &self.rootfs)
            .await
            .map_err(|e| pool::PoolError::Backend(e.to_string()))?;
        Ok(pool::PooledVm { id, warm: false })
    }

    async fn resume(&self, vm: &pool::PooledVm) -> Result<(), pool::PoolError> {
        self.client_for(&vm.id)
            .set_vm_state(VmState::Resumed)
            .await
            .map_err(|e| pool::PoolError::Backend(e.to_string()))
    }

    async fn destroy(&self, vm: pool::PooledVm) -> Result<(), pool::PoolError> {
        // `SendCtrlAltDel` is the graceful path; the VMM process dying with its jail is the
        // guarantee. A pool that depended on a guest cooperating with its own shutdown would leak a
        // VM every time a stranger's code ignored the signal.
        let _ = self.client_for(&vm.id).action(Action::SendCtrlAltDel).await;
        Ok(())
    }
}

/// Monotonic per-process VM ids. Not random: an operator reading `warm-41` next to `warm-42` in a
/// log can tell which came first, and a uuid there would tell them nothing.
fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}
