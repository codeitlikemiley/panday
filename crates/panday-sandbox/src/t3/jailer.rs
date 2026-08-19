//! The jailer's command line, and the cgroup caps that go with it (docs/14 M14.5).
//!
//! Firecracker ships `jailer` precisely so that the VMM process is not the thing standing between a
//! guest and the host: it chroots, drops to an unprivileged uid/gid, enters a network namespace, and
//! applies cgroup limits *before* exec'ing the VMM. Getting its arguments right is the whole of the
//! host-side security story for T3, which is why they are built by a function with tests rather
//! than assembled in a string somewhere.
//!
//! Nothing here needs KVM to be correct — a wrong `--uid` is wrong on any machine — so it is tested
//! everywhere and only *run* on Linux.

use std::path::{Path, PathBuf};

/// Resource caps applied by the jailer through cgroup v2.
///
/// docs/14: "jailer + cgroup v2 caps (cpu, mem, pids, io)". Every one of these has a documented
/// reason to exist: without `pids` a fork bomb inside the guest takes the host's process table with
/// it, and without `io` one sandbox's disk thrash is every other sandbox's latency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    /// Hundredths of a CPU: 200 = two cores' worth.
    pub cpu_percent: u32,
    pub mem_mib: u32,
    pub pids_max: u32,
    /// Bytes per second, read and write combined.
    pub io_bytes_per_sec: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            cpu_percent: 200,
            mem_mib: 2048,
            pids_max: 512,
            io_bytes_per_sec: 100 * 1024 * 1024,
        }
    }
}

impl Caps {
    /// `cgroup_key=value` arguments, in the form the jailer takes them.
    pub fn args(&self, cgroup_version: u8) -> Vec<String> {
        let mut out = vec![format!("--cgroup-version"), cgroup_version.to_string()];
        for (key, value) in [
            ("cpu.max", format!("{} 100000", self.cpu_percent * 1000)),
            (
                "memory.max",
                format!("{}", self.mem_mib as u64 * 1024 * 1024),
            ),
            ("pids.max", format!("{}", self.pids_max)),
        ] {
            out.push("--cgroup".into());
            out.push(format!("{key}={value}"));
        }
        out
    }
}

/// Everything the jailer needs to know.
#[derive(Debug, Clone)]
pub struct JailerConfig {
    pub jailer_binary: PathBuf,
    pub firecracker_binary: PathBuf,
    /// Unique per VM; also names the chroot directory the jailer builds.
    pub id: String,
    /// The unprivileged user the VMM runs as. **Never 0** — the point of the jailer is that a
    /// Firecracker escape lands somewhere with no privileges, and running it as root would make the
    /// chroot the only thing standing between a guest and the host.
    pub uid: u32,
    pub gid: u32,
    pub chroot_base: PathBuf,
    /// A pre-created network namespace, so the guest's only route out is the tap device the egress
    /// proxy owns (docs/14 §network).
    pub netns: Option<String>,
    pub caps: Caps,
}

/// Why a jail configuration was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JailError {
    #[error("the jailer must not run the VMM as root (uid 0)")]
    RootUid,
    #[error("`{0}` is not a usable VM id: letters, digits, dashes and underscores only")]
    BadId(String),
}

impl JailerConfig {
    pub fn new(id: impl Into<String>, uid: u32, gid: u32) -> Self {
        Self {
            jailer_binary: PathBuf::from("/usr/bin/jailer"),
            firecracker_binary: PathBuf::from("/usr/bin/firecracker"),
            id: id.into(),
            uid,
            gid,
            chroot_base: PathBuf::from("/srv/jailer"),
            netns: None,
            caps: Caps::default(),
        }
    }

    /// Where the jailer will put this VM's chroot, and therefore where its socket appears.
    ///
    /// The layout is the jailer's, not ours: `<base>/firecracker/<id>/root`. Recomputing it here
    /// rather than guessing later is what lets the API client find the socket without parsing the
    /// jailer's output.
    pub fn chroot(&self) -> PathBuf {
        self.chroot_base
            .join("firecracker")
            .join(&self.id)
            .join("root")
    }

    /// The API socket, as seen from *outside* the chroot.
    pub fn api_socket(&self) -> PathBuf {
        self.chroot().join("run/firecracker.socket")
    }

    /// The full argument vector, jailer first, VMM arguments after the `--` separator.
    pub fn args(&self) -> Result<Vec<String>, JailError> {
        if self.uid == 0 || self.gid == 0 {
            // A Firecracker escape should land somewhere with no privileges. Root inside the
            // chroot makes the chroot the only barrier, and chroots are not a security boundary.
            return Err(JailError::RootUid);
        }
        if self.id.is_empty()
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            // The id becomes a path component. A `..` in it would put the chroot somewhere the
            // operator did not choose.
            return Err(JailError::BadId(self.id.clone()));
        }

        let mut args = vec![
            "--id".to_string(),
            self.id.clone(),
            "--exec-file".into(),
            self.firecracker_binary.display().to_string(),
            "--uid".into(),
            self.uid.to_string(),
            "--gid".into(),
            self.gid.to_string(),
            "--chroot-base-dir".into(),
            self.chroot_base.display().to_string(),
        ];
        if let Some(netns) = &self.netns {
            args.push("--netns".into());
            args.push(format!("/var/run/netns/{netns}"));
        }
        args.extend(self.caps.args(2));

        // Everything after `--` is Firecracker's own. The socket path is relative to the chroot,
        // because by the time the VMM runs, the chroot *is* its root.
        args.push("--".into());
        args.push("--api-sock".into());
        args.push("/run/firecracker.socket".into());
        Ok(args)
    }

    /// The command to spawn, ready to run. Split out so a test can inspect it without executing it.
    pub fn command(&self) -> Result<(PathBuf, Vec<String>), JailError> {
        Ok((self.jailer_binary.clone(), self.args()?))
    }
}

/// Whether this machine can run T3 at all.
///
/// Checked as a file rather than by trying: a missing `/dev/kvm` is the ordinary case on a laptop
/// and on most CI, and the useful behaviour is one clear sentence rather than a boot that fails
/// three steps later with something about an ioctl.
pub fn kvm_available() -> bool {
    Path::new("/dev/kvm").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vmm_never_runs_as_root() {
        // The jailer's entire purpose. A Firecracker escape has to land somewhere powerless.
        let mut config = JailerConfig::new("vm-1", 0, 1000);
        assert_eq!(config.args(), Err(JailError::RootUid));
        config.uid = 1000;
        config.gid = 0;
        assert_eq!(config.args(), Err(JailError::RootUid));
    }

    #[test]
    fn an_id_that_is_a_path_traversal_is_refused() {
        // The id becomes a path component under the chroot base.
        assert!(matches!(
            JailerConfig::new("../../etc", 1000, 1000).args(),
            Err(JailError::BadId(_))
        ));
        assert!(matches!(
            JailerConfig::new("", 1000, 1000).args(),
            Err(JailError::BadId(_))
        ));
        assert!(JailerConfig::new("vm_1-abc", 1000, 1000).args().is_ok());
    }

    #[test]
    fn the_socket_path_matches_the_jailers_own_layout() {
        // Computed, not guessed: this is how the API client finds the socket without parsing
        // anything the jailer printed.
        let config = JailerConfig::new("vm-7", 1000, 1000);
        assert_eq!(
            config.api_socket(),
            PathBuf::from("/srv/jailer/firecracker/vm-7/root/run/firecracker.socket")
        );
    }

    #[test]
    fn every_cap_docs_names_is_actually_applied() {
        // Without `pids` a fork bomb in the guest takes the host's process table; without `memory`
        // one sandbox evicts every other.
        let args = Caps::default().args(2).join(" ");
        assert!(args.contains("--cgroup-version 2"), "{args}");
        assert!(args.contains("cpu.max="), "{args}");
        assert!(args.contains("memory.max=2147483648"), "{args}");
        assert!(args.contains("pids.max=512"), "{args}");
    }

    #[test]
    fn a_netns_is_passed_as_a_path_because_that_is_what_the_jailer_takes() {
        let mut config = JailerConfig::new("vm-1", 1000, 1000);
        config.netns = Some("sbx1".into());
        let args = config.args().unwrap();
        let at = args.iter().position(|a| a == "--netns").expect("--netns");
        assert_eq!(args[at + 1], "/var/run/netns/sbx1");
    }

    #[test]
    fn the_vmm_arguments_come_after_the_separator() {
        // Anything before `--` is the jailer's; anything after is Firecracker's. Getting this wrong
        // silently hands the jailer a flag it ignores.
        let args = JailerConfig::new("vm-1", 1000, 1000).args().unwrap();
        let sep = args.iter().position(|a| a == "--").expect("separator");
        assert!(args[..sep].iter().any(|a| a == "--chroot-base-dir"));
        assert_eq!(args[sep + 1], "--api-sock");
        // Relative to the chroot: by the time the VMM runs, the chroot is its root.
        assert_eq!(args[sep + 2], "/run/firecracker.socket");
    }
}
