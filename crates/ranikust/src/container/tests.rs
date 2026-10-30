use std::collections::BTreeMap;

use super::*;

/// A host made of the given files and `container` variable.
#[derive(Default)]
struct Fake {
    files: BTreeMap<&'static str, &'static str>,
    env: Option<&'static str>,
}

impl Fake {
    fn file(mut self, path: &'static str, content: &'static str) -> Self {
        self.files.insert(path, content);
        self
    }
}

impl Host for Fake {
    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    fn read(&self, path: &str) -> String {
        self.files
            .get(path)
            .map(|c| c.trim().to_owned())
            .unwrap_or_default()
    }

    fn container_env(&self) -> Option<String> {
        self.env.map(str::to_owned)
    }
}

/// A plain host: cgroup v2, no marker file, ext4 on `/`.
fn host() -> Fake {
    Fake::default()
        .file(
            "/proc/self/cgroup",
            "0::/user.slice/user-1000.slice/session-2.scope\n",
        )
        .file("/proc/1/cmdline", "/sbin/init\0splash\0")
        .file(
            "/proc/mounts",
            "/dev/nvme0n1p2 / ext4 rw,relatime 0 0\nproc /proc proc rw 0 0\n",
        )
}

#[test]
fn a_plain_host_is_not_a_container() {
    assert_eq!(detect(&host()), None);
}

#[test]
fn marker_files_name_the_runtime_in_kanikos_order() {
    assert_eq!(
        detect(&host().file("/.dockerenv", "")),
        Some(Runtime::Docker)
    );
    let both = host()
        .file("/.dockerenv", "")
        .file("/run/.containerenv", "");
    assert_eq!(detect(&both), Some(Runtime::Podman));
    let pod = host().file("/var/run/secrets/kubernetes.io/serviceaccount", "");
    assert_eq!(detect(&pod), Some(Runtime::Kubernetes));
}

#[test]
fn cgroup_names_and_the_first_match_wins() {
    let docker = host().file("/proc/self/cgroup", "0::/system.slice/docker-1a2b.scope");
    assert_eq!(detect(&docker), Some(Runtime::Docker));
    // "lxc-libvirt" contains "lxc", which comes first.
    let lxc = host().file("/proc/self/cgroup", "0::/machine/lxc-libvirt/x");
    assert_eq!(detect(&lxc), Some(Runtime::Lxc));
    let pod = host().file("/proc/self/cgroup", "0::/kubepods/besteffort/pod1");
    assert_eq!(detect(&pod), Some(Runtime::Kubernetes));
}

#[test]
fn the_remaining_signals_in_order() {
    let vz = host().file("/proc/vz", "");
    assert_eq!(detect(&vz), Some(Runtime::OpenVz));
    assert_eq!(detect(&vz.file("/proc/bc", "")), None, "an OpenVZ host");
    let gvisor = host().file("/__runsc_containers__", "");
    assert_eq!(detect(&gvisor), Some(Runtime::GVisor));
    let firejail = host().file("/proc/1/cmdline", "firejail\0--noprofile\0");
    assert_eq!(detect(&firejail), Some(Runtime::Firejail));
    let wsl = host().file("/proc/version_signature", "Microsoft 4.4.0-19041");
    assert_eq!(detect(&wsl), Some(Runtime::Wsl));
    let env = Fake {
        env: Some("podman"),
        ..host()
    };
    assert_eq!(detect(&env), Some(Runtime::Podman));
    let nspawn = host().file("/run/systemd/container", "systemd-nspawn\n");
    assert_eq!(detect(&nspawn), Some(Runtime::Nspawn));
}

#[test]
fn an_overlay_root_counts_only_on_the_first_mount_line() {
    let overlay = host().file(
        "/proc/mounts",
        "overlay / overlay rw,lowerdir=/l,upperdir=/u 0 0\nproc /proc proc rw 0 0\n",
    );
    assert_eq!(detect(&overlay), Some(Runtime::Kubernetes));
    let later = host().file(
        "/proc/mounts",
        "/dev/sda1 / ext4 rw 0 0\noverlay / overlay rw 0 0\n",
    );
    assert_eq!(detect(&later), None);
    let elsewhere = host().file("/proc/mounts", "overlay /var/lib/x overlay rw 0 0\n");
    assert_eq!(detect(&elsewhere), None);
}

#[test]
fn an_empty_container_variable_is_not_a_runtime() {
    let env = Fake {
        env: Some("  "),
        ..host()
    };
    assert_eq!(detect(&env), None);
}
