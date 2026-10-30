//! `RUN --mount=type=cache|secret` as the sandbox's binds, as BuildKit
//! makes them (ADR-0032): cache mounts leased from the store with their
//! `sharing=`, secrets written to a private file or passed as one
//! variable of the `RUN`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};

use rnk_dockerfile::{Mount, MountType, Sharing};
use rnk_rootfs::{OpenHow, RootPath};
use rnk_sandbox::{BindMount, MountKind};
use rnk_store::{CacheLease, CacheSharing, Store};

use crate::error::{Error, Result};
use crate::sources;

/// The largest secret, as BuildKit's (`session/secrets/secretsprovider`
/// `MaxSecretSize`): 500 KiB.
pub const MAX_SECRET_SIZE: u64 = 500 * 1024;

/// Where a `--secret` comes from, as buildctl's and kaniko's `--secret`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Secret {
    /// A host file, read when a `RUN` mounts it; an absolute path.
    File(PathBuf),
    /// A variable of ranikust's own environment, read when a `RUN` mounts
    /// it; unset reads as empty, as BuildKit's `os.Getenv`.
    Env(OsString),
}

impl Secret {
    /// The secret's bytes.
    fn read(&self, id: &str) -> Result<Vec<u8>> {
        let data = match self {
            Self::File(path) => {
                let f = std::fs::File::open(path).map_err(Error::io("open the secret", path))?;
                let mut data = Vec::new();
                // One byte more than the limit tells a file that grew since
                // the start-up check.
                f.take(MAX_SECRET_SIZE + 1)
                    .read_to_end(&mut data)
                    .map_err(Error::io("read the secret", path))?;
                data
            }
            Self::Env(name) => std::env::var_os(name).unwrap_or_default().into_vec(),
        };
        if u64::try_from(data.len()).unwrap_or(u64::MAX) > MAX_SECRET_SIZE {
            return Err(Error::Invalid(format!(
                "secret {id:?} is larger than {MAX_SECRET_SIZE} bytes"
            )));
        }
        Ok(data)
    }
}

/// What a `RUN`'s mounts hold until it ends: the binds and variables it
/// gets, the cache leases and the secret files.
#[derive(Debug, Default)]
pub(crate) struct RunMounts {
    pub(crate) binds: Vec<BindMount>,
    /// Secrets passed with `env=`: never in the image config or a log.
    pub(crate) env: Vec<(OsString, OsString)>,
    leases: Vec<CacheLease>,
    files: Vec<tempfile::NamedTempFile>,
}

impl RunMounts {
    /// Ends the mounts once the `RUN`'s processes are gone: removes the
    /// secret files and ends the cache leases. A crash leaves the files in
    /// the build directory, which the next start removes.
    pub(crate) fn finish(self) -> Result<()> {
        drop(self.leases);
        for f in self.files {
            let path = f.path().to_owned();
            f.close()
                .map_err(Error::io("remove the secret file", path))?;
        }
        Ok(())
    }
}

/// `RUN --mount` values as binds; the mounts of the types this phase lacks
/// are refused. `cancelled` ends a wait for a `locked` cache.
pub(crate) fn run_mounts(
    mounts: &[Mount],
    workdir: &Path,
    store: &Store,
    secrets: &BTreeMap<String, Secret>,
    cancelled: &dyn Fn() -> bool,
) -> Result<RunMounts> {
    let mut out = RunMounts::default();
    for m in mounts {
        match m.kind {
            MountType::Cache => {
                if !m.from.is_empty() {
                    return Err(Error::Invalid(
                        "RUN --mount=type=cache,from= is not supported yet (Phase 5)".to_owned(),
                    ));
                }
                if m.target.is_empty() {
                    return Err(Error::Invalid(
                        "RUN --mount=type=cache needs a target".to_owned(),
                    ));
                }
                let sharing = match m.sharing {
                    None | Some(Sharing::Shared) => CacheSharing::Shared,
                    Some(Sharing::Locked) => CacheSharing::Locked,
                    Some(Sharing::Private) => CacheSharing::Private,
                };
                let lease = store.cache_lease(&cache_key(m), sharing, cancelled)?;
                if lease.created() {
                    set_cache_owner(lease.path(), m)?;
                }
                out.binds.push(BindMount {
                    source: lease.path().to_owned(),
                    target: mount_target(&m.target, workdir),
                    kind: MountKind::Directory,
                    read_only: m.read_only,
                });
                out.leases.push(lease);
            }
            MountType::Secret => secret_mount(m, workdir, store, secrets, &mut out)?,
            MountType::Bind | MountType::Tmpfs | MountType::Ssh => {
                return Err(Error::Invalid(format!(
                    "RUN --mount=type={} is not supported yet (Phase 5)",
                    m.kind
                )));
            }
        }
    }
    Ok(out)
}

/// A `RUN --mount` target inside the root: absolute, or relative to the
/// working directory; cleaned, never above `/`.
pub(crate) fn mount_target(t: &str, workdir: &Path) -> PathBuf {
    let joined = if t.starts_with('/') {
        PathBuf::from(t)
    } else {
        workdir.join(t)
    };
    PathBuf::from("/").join(sources::clean_source(&joined.to_string_lossy()))
}

/// The id of a cache mount: `id=`, else its target cleaned as Go's
/// `path.Clean`, as BuildKit (`convert_runmount.go`).
pub(crate) fn cache_id(m: &Mount) -> String {
    if m.cache_id.is_empty() {
        go_clean(&m.target)
    } else {
        m.cache_id.clone()
    }
}

/// The store key of a cache mount. BuildKit keys a cache by its id and by
/// the base its `uid`, `gid` and `mode` make (`mount.go` `getRefCacheDir`,
/// `convert_runmount.go` `setCacheUIDGID`), so caches of one id with other
/// owners or modes are other caches.
fn cache_key(m: &Mount) -> String {
    let id = cache_id(m);
    if m.uid.is_none() && m.gid.is_none() && m.mode.is_none() {
        return id;
    }
    let (uid, gid, mode) = cache_owner(m);
    format!("{id}\0uid={uid},gid={gid},mode={mode:o}")
}

/// A cache's owner and mode when the mount sets any of them, BuildKit's
/// defaults filling the rest: uid and gid 0, mode `0755`.
fn cache_owner(m: &Mount) -> (u32, u32, u32) {
    (
        m.uid.unwrap_or(0),
        m.gid.unwrap_or(0),
        m.mode.unwrap_or(0o755),
    )
}

/// Applies a new cache's owner and mode, once, as BuildKit's `mkdir` of the
/// cache's base: through a descriptor of the directory itself, never by a
/// path a symlink could redirect. Only permission bits and the sticky bit
/// are taken from `mode`: no set-id bit on a host directory.
fn set_cache_owner(dir: &Path, m: &Mount) -> Result<()> {
    if m.mode.is_none() && m.uid.is_none() && m.gid.is_none() {
        return Ok(());
    }
    let (uid, gid, mode) = cache_owner(m);
    let root = RootPath::open(dir)?;
    let fd = root.open_at(".", &OpenHow::read().directory().no_follow())?;
    set_owner(&fd, uid, gid).map_err(Error::io("chown", dir))?;
    rustix::fs::fchmod(&fd, rustix::fs::Mode::from_raw_mode(mode & 0o1777))
        .map_err(|e| Error::io("chmod", dir)(e.into()))?;
    Ok(())
}

/// A `RUN --mount=type=secret`, as BuildKit's `dispatchSecret` and secret
/// mount (`convert_secrets.go`, `secretmount_unix.go`):
///
/// - the id is `id=`, else `source=`, else the target's last name;
/// - a missing secret is an error when `required`, else the mount is left
///   out;
/// - with `env=`, the value is that variable of this `RUN` only;
/// - without `env=`, or with a `target=`, the value is a file at the
///   target, else at `/run/secrets/<id>`: a private copy, mode `mode`
///   (default `0400`, permission bits only) and owner `uid`:`gid`
///   (default root), bound read-only.
fn secret_mount(
    m: &Mount,
    workdir: &Path,
    store: &Store,
    secrets: &BTreeMap<String, Secret>,
    out: &mut RunMounts,
) -> Result<()> {
    let id = if !m.source.is_empty() {
        m.source.clone()
    } else if !m.cache_id.is_empty() {
        m.cache_id.clone()
    } else {
        Path::new(&m.target)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    if id.is_empty() {
        return Err(Error::Invalid(
            "RUN --mount=type=secret needs an id, a source or a target".to_owned(),
        ));
    }
    let Some(secret) = secrets.get(&id) else {
        if m.required {
            return Err(Error::Invalid(format!(
                "secret {id:?} is required but was not given (--secret)"
            )));
        }
        return Ok(());
    };
    let data = secret.read(&id)?;
    if let Some(name) = &m.env {
        if name.is_empty() || name.contains('=') {
            return Err(Error::Invalid(format!(
                "RUN --mount=type=secret,env={name:?} is not a variable name"
            )));
        }
        out.env
            .push((name.into(), OsString::from_vec(data.clone())));
        if m.target.is_empty() {
            return Ok(());
        }
    }
    let target = if m.target.is_empty() {
        let base = Path::new(&id)
            .file_name()
            .map_or_else(|| id.clone(), |n| n.to_string_lossy().into_owned());
        PathBuf::from("/run/secrets").join(base)
    } else {
        mount_target(&m.target, workdir)
    };
    let file = write_secret(store.build_dir(), &data, m)?;
    out.binds.push(BindMount {
        source: file.path().to_owned(),
        target,
        kind: MountKind::File,
        read_only: true,
    });
    out.files.push(file);
    Ok(())
}

/// The secret's private copy in the build directory (`0700`, root's): made
/// `0600`, written, then given the mount's owner and mode through its
/// descriptor. BuildKit writes it to a private tmpfs instead
/// [open: Q-230].
fn write_secret(dir: &Path, data: &[u8], m: &Mount) -> Result<tempfile::NamedTempFile> {
    let mut f = tempfile::Builder::new()
        .prefix(".secret-")
        .tempfile_in(dir)
        .map_err(Error::io("create a secret file in", dir))?;
    f.write_all(data)
        .map_err(Error::io("write", f.path().to_owned()))?;
    set_owner(f.as_file(), m.uid.unwrap_or(0), m.gid.unwrap_or(0))
        .map_err(Error::io("chown", f.path().to_owned()))?;
    rustix::fs::fchmod(
        f.as_file(),
        rustix::fs::Mode::from_raw_mode(m.mode.unwrap_or(0o400) & 0o777),
    )
    .map_err(|e| Error::io("chmod", f.path().to_owned())(e.into()))?;
    Ok(f)
}

/// Gives `fd` the owner `uid`:`gid`, unless it has it already: a file this
/// process just made is its own, and only a change needs `CAP_CHOWN`.
fn set_owner(fd: impl std::os::fd::AsFd, uid: u32, gid: u32) -> std::io::Result<()> {
    let st = rustix::fs::fstat(&fd)?;
    if (st.st_uid, st.st_gid) == (uid, gid) {
        return Ok(());
    }
    rustix::fs::fchown(
        &fd,
        Some(rustix::fs::Uid::from_raw(uid)),
        Some(rustix::fs::Gid::from_raw(gid)),
    )?;
    Ok(())
}

/// Go's `path.Clean` for a slash path: `//`, `.` and `..` resolved
/// lexically, a rooted path never above `/`, a relative one keeping its
/// leading `..`.
fn go_clean(p: &str) -> String {
    let rooted = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

#[cfg(test)]
mod tests;
