// Derived from kaniko, https://github.com/osscontainertools/kaniko,
// pkg/util/proc/proc.go at commit 34ba9eadf (itself ported there from
// genuinetools/bpfd).
// Copyright 2022 Google LLC. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: the host's files and environment behind a trait,
// so the rules are tested without a container.

//! Whether ranikust runs inside a container, decided as kaniko decides it
//! before building (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:240-245`,
//! `pkg/util/proc/proc.go:78-149`, ADR-0040). Kaniko mode builds on the
//! root `/` only there; elsewhere in its state directory (ADR-0050).

use std::fmt;
use std::path::Path;

/// A container runtime kaniko recognises, in kaniko's matching order: the
/// first whose name appears in a string wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Runtime {
    Docker,
    Rkt,
    Nspawn,
    Lxc,
    LxcLibvirt,
    OpenVz,
    Kubernetes,
    Garden,
    Podman,
    GVisor,
    Firejail,
    Wsl,
}

impl Runtime {
    /// Every runtime, in the order kaniko matches them.
    const ALL: [Runtime; 12] = [
        Runtime::Docker,
        Runtime::Rkt,
        Runtime::Nspawn,
        Runtime::Lxc,
        Runtime::LxcLibvirt,
        Runtime::OpenVz,
        Runtime::Kubernetes,
        Runtime::Garden,
        Runtime::Podman,
        Runtime::GVisor,
        Runtime::Firejail,
        Runtime::Wsl,
    ];

    /// kaniko's name for the runtime, also what is matched.
    fn name(self) -> &'static str {
        match self {
            Runtime::Docker => "docker",
            Runtime::Rkt => "rkt",
            Runtime::Nspawn => "systemd-nspawn",
            Runtime::Lxc => "lxc",
            Runtime::LxcLibvirt => "lxc-libvirt",
            Runtime::OpenVz => "openvz",
            Runtime::Kubernetes => "kube",
            Runtime::Garden => "garden",
            Runtime::Podman => "podman",
            Runtime::GVisor => "gvisor",
            Runtime::Firejail => "firejail",
            Runtime::Wsl => "wsl",
        }
    }

    /// The first runtime whose name `text` contains (kaniko's
    /// `getContainerRuntime`).
    fn find_in(text: &str) -> Option<Runtime> {
        if text.trim().is_empty() {
            return None;
        }
        Runtime::ALL.into_iter().find(|r| text.contains(r.name()))
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What the detection reads: files of the host and one variable.
pub(crate) trait Host {
    /// Whether `path` exists; an error other than "not found" counts as
    /// existing, as Go's `!os.IsNotExist(err)`.
    fn exists(&self, path: &str) -> bool;
    /// The content of `path`, trimmed; empty when missing or unreadable.
    fn read(&self, path: &str) -> String;
    /// The environment variable `container`.
    fn container_env(&self) -> Option<String>;
}

/// The real host.
pub(crate) struct Real;

impl Host for Real {
    fn exists(&self, path: &str) -> bool {
        match std::fs::metadata(Path::new(path)) {
            Ok(_) => true,
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        }
    }

    fn read(&self, path: &str) -> String {
        std::fs::read(path)
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
            .unwrap_or_default()
    }

    fn container_env(&self) -> Option<String> {
        std::env::var("container").ok()
    }
}

/// The runtime ranikust runs in, `None` outside a container: kaniko's
/// `GetContainerRuntime(0, 0)` checks, in its order.
///
/// This heuristic is what keeps a build off the host's `/` (which kaniko
/// mode extracts over, and empties between stages): outside a container
/// the build runs in the state directory instead (ADR-0050); in one no
/// flag is needed (ADR-0048), accepting that a host matching the heuristic
/// by accident is not protected.
pub(crate) fn detect(host: &impl Host) -> Option<Runtime> {
    // Container-specific files.
    for (runtime, file) in [
        (Runtime::Podman, "/run/.containerenv"),
        (Runtime::Docker, "/.dockerenv"),
        (
            Runtime::Kubernetes,
            "/var/run/secrets/kubernetes.io/serviceaccount",
        ),
    ] {
        if host.exists(file) {
            return Some(runtime);
        }
    }
    if let Some(r) = Runtime::find_in(&host.read("/proc/self/cgroup")) {
        return Some(r);
    }
    // `/proc/vz` exists inside and outside an OpenVZ container, `/proc/bc`
    // only outside.
    if host.exists("/proc/vz") && !host.exists("/proc/bc") {
        return Some(Runtime::OpenVz);
    }
    if host.exists("/__runsc_containers__") {
        return Some(Runtime::GVisor);
    }
    // firejail runs as pid 1 under its own name.
    if let Some(r) = Runtime::find_in(&host.read("/proc/1/cmdline")) {
        return Some(r);
    }
    if host
        .read("/proc/version_signature")
        .starts_with("Microsoft")
    {
        return Some(Runtime::Wsl);
    }
    if let Some(r) = host.container_env().and_then(|v| Runtime::find_in(&v)) {
        return Some(r);
    }
    if let Some(r) = Runtime::find_in(&host.read("/run/systemd/container")) {
        return Some(r);
    }
    // An overlay mount on `/` may mean containerd or another runtime: Go's
    // `regexp.MatchString("^[^ ]+ / overlay", mounts)`, which without the
    // multi-line flag looks at the first line only.
    let mounts = host.read("/proc/mounts");
    let first = mounts.lines().next().unwrap_or("");
    let mut fields = first.split(' ');
    if let (Some(device), Some("/"), Some(fs)) = (fields.next(), fields.next(), fields.next())
        && !device.is_empty()
        && fs.starts_with("overlay")
    {
        return Some(Runtime::Kubernetes);
    }
    None
}

#[cfg(test)]
mod tests;
