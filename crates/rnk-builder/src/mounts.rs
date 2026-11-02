//! `RUN --mount=type=cache|secret` for the sandbox, as BuildKit makes them
//! (ADR-0032): cache mounts leased from the store with their `sharing=`,
//! secrets handed to the sandbox, which writes them on a tmpfs of the
//! `RUN`'s own, or passed as one variable of the `RUN`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rnk_dockerfile::{Mount, MountType, Sharing};
use rnk_sandbox::{BindMount, MountKind, SecretFile};
use rnk_store::{CacheLease, CacheOwner, CacheSharing, Store};

use crate::error::{Error, Result};
use crate::sources;

/// The largest secret, as BuildKit's (`session/secrets/secretsprovider`
/// `MaxSecretSize`): 500 KiB.
pub const MAX_SECRET_SIZE: u64 = 500 * 1024;

/// Where a `--secret` comes from, as buildctl's and kaniko's `--secret`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Secret {
    /// A host file; an absolute path.
    File(PathBuf),
    /// A variable of ranikust's own environment; unset reads as empty, as
    /// BuildKit's `os.Getenv`.
    Env(OsString),
}

impl Secret {
    /// Reads the secret, once, at the start of the build (ADR-0078): a file
    /// must exist, be a regular file and fit [`MAX_SECRET_SIZE`], as
    /// BuildKit's store checks at the start. Its path is never opened
    /// again: a `RUN` that replaces the file, or a symlink in its place,
    /// changes nothing a later `RUN` gets.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the file cannot be opened or read,
    /// [`Error::Invalid`] when it is not a regular file or the secret is
    /// too large.
    pub fn load(&self, id: &str) -> Result<SecretValue> {
        // Every buffer the bytes pass through is wiped when it is dropped,
        // on error paths too; none is grown, so none leaves an old block
        // behind.
        let raw = match self {
            Self::File(path) => {
                let mut f = open_secret(id, path)?;
                // One byte more than the limit tells a file that grew since
                // its size was read. A plain read loop into this buffer: no
                // other buffer holds the bytes.
                let mut buf = Wiped(vec![
                    0u8;
                    usize::try_from(MAX_SECRET_SIZE + 1)
                        .unwrap_or(usize::MAX)
                ]);
                let mut n = 0;
                while n < buf.0.len() {
                    match f.read(&mut buf.0[n..]) {
                        Ok(0) => break,
                        Ok(k) => n += k,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(Error::io("read the secret", path)(e)),
                    }
                }
                buf.0.truncate(n);
                buf
            }
            Self::Env(name) => Wiped(std::env::var_os(name).unwrap_or_default().into_vec()),
        };
        if u64::try_from(raw.0.len()).unwrap_or(u64::MAX) > MAX_SECRET_SIZE {
            return Err(Error::Invalid(format!(
                "secret {id:?} is larger than {MAX_SECRET_SIZE} bytes"
            )));
        }
        Ok(SecretValue {
            bytes: Arc::<[u8]>::from(raw.0.as_slice()),
        })
    }
}

/// Overwrites `bytes` with zeros in a way the compiler keeps: the memory is
/// about to be freed, so plain stores could be dropped.
fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference to a byte.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// A buffer holding a secret's bytes, wiped when dropped: its whole
/// allocation, the part past its length included.
struct Wiped(Vec<u8>);

impl Drop for Wiped {
    fn drop(&mut self) {
        let len = self.0.capacity();
        // Zero-filling up to the capacity never reallocates.
        self.0.resize(len, 0);
        wipe(&mut self.0);
    }
}

/// A secret's bytes, read once by [`Secret::load`] and shared by every
/// `RUN` that mounts it. `Debug` never shows them. When the last holder
/// drops them they are overwritten with zeros, as is every buffer
/// [`Secret::load`] read them through. Not wiped: the copies made for a
/// `RUN` (its variable, the file on its tmpfs), and the value itself when
/// the build ends with another thread still holding it, which never drops
/// it ([open: Q-444]).
pub struct SecretValue {
    bytes: Arc<[u8]>,
}

impl SecretValue {
    /// A value from bytes already in memory (tests, callers that read a
    /// secret themselves).
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    /// Its length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether it is empty (an unset variable).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretValue(<{} bytes hidden>)", self.bytes.len())
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        // Only the last holder can write: a `RUN`'s mount still holding the
        // allocation keeps it until it drops it.
        if let Some(bytes) = Arc::get_mut(&mut self.bytes) {
            wipe(bytes);
        }
    }
}

/// Opens a secret file without blocking on a FIFO or a device, and checks
/// that it is a regular file of at most [`MAX_SECRET_SIZE`] bytes.
fn open_secret(id: &str, path: &Path) -> Result<std::fs::File> {
    use rustix::fs::{FileType, Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| Error::io("open the secret", path)(e.into()))?;
    let st = rustix::fs::fstat(&fd).map_err(|e| Error::io("stat the secret", path)(e.into()))?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(Error::Invalid(format!(
            "secret {id:?}: {} is not a regular file",
            path.display()
        )));
    }
    if u64::try_from(st.st_size).unwrap_or(u64::MAX) > MAX_SECRET_SIZE {
        return Err(Error::Invalid(format!(
            "secret {id:?}: {} is larger than {MAX_SECRET_SIZE} bytes",
            path.display()
        )));
    }
    Ok(std::fs::File::from(fd))
}

/// What a `RUN`'s mounts hold until it ends: the binds, secret files and
/// variables it gets, and the cache leases. Its `Debug` hides the variables'
/// values and the secrets' bytes.
#[derive(Default)]
pub(crate) struct RunMounts {
    pub(crate) binds: Vec<BindMount>,
    /// Secrets the sandbox writes on the `RUN`'s own tmpfs.
    pub(crate) secrets: Vec<SecretFile>,
    /// Secrets passed with `env=`: never in the image config or a log.
    pub(crate) env: Vec<(OsString, OsString)>,
    leases: Vec<CacheLease>,
}

impl std::fmt::Debug for RunMounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<&OsString> = self.env.iter().map(|(k, _)| k).collect();
        f.debug_struct("RunMounts")
            .field("binds", &self.binds)
            .field("secrets", &self.secrets)
            .field("env (names)", &env)
            .field("leases", &self.leases)
            .finish()
    }
}

impl RunMounts {
    /// Ends the cache leases once the `RUN`'s processes are gone. Nothing
    /// of a secret is left: it lived on the `RUN`'s tmpfs.
    pub(crate) fn finish(self) {
        drop(self.leases);
    }
}

/// `RUN --mount` values for the sandbox; the mounts of the types this phase
/// lacks are refused. Cache leases are taken in the order of their keys, so
/// two `RUN`s with the same `locked` caches cannot wait for each other, and
/// a key mounted twice by one `RUN` uses one lease, as BuildKit shares one
/// ref per key within an exec (`mount.go` `getRefCacheDir`). `cancelled`
/// ends a wait for a `locked` cache.
pub(crate) fn run_mounts(
    mounts: &[Mount],
    workdir: &Path,
    store: &Store,
    secrets: &BTreeMap<String, SecretValue>,
    cancelled: &dyn Fn() -> bool,
) -> Result<RunMounts> {
    let mut out = RunMounts::default();
    let mut caches: Vec<(String, &Mount)> = Vec::new();
    check_supported(mounts)?;
    // A required secret that was not given fails the `RUN` before any
    // mount of it is made.
    if let Some(id) = mounts
        .iter()
        .filter(|m| m.kind == MountType::Secret && m.required)
        .map(secret_id)
        .find(|id| !secrets.contains_key(id))
    {
        return Err(Error::Invalid(format!(
            "secret {id:?} is required but was not given (--secret)"
        )));
    }
    for m in mounts {
        match m.kind {
            MountType::Cache => {
                caches.push((cache_key(m), m));
            }
            MountType::Secret => secret_mount(m, workdir, secrets, &mut out)?,
            // Refused by `check_supported` above; refused here too, so that
            // relaxing that check can never drop a mount silently.
            MountType::Bind | MountType::Tmpfs | MountType::Ssh => {
                return Err(Error::Invalid(format!(
                    "RUN --mount=type={} is not supported yet (Phase 5)",
                    m.kind
                )));
            }
        }
    }
    let mut order: Vec<usize> = (0..caches.len()).collect();
    order.sort_by(|&a, &b| caches[a].0.cmp(&caches[b].0));
    let mut leased: BTreeMap<&str, PathBuf> = BTreeMap::new();
    let mut sources: Vec<Option<PathBuf>> = vec![None; caches.len()];
    for i in order {
        let (key, m) = &caches[i];
        let path = match leased.get(key.as_str()) {
            Some(path) => path.clone(),
            None => {
                let sharing = match m.sharing {
                    None | Some(Sharing::Shared) => CacheSharing::Shared,
                    Some(Sharing::Locked) => CacheSharing::Locked,
                    Some(Sharing::Private) => CacheSharing::Private,
                };
                let lease = store.cache_lease(key, sharing, cache_owner(m), cancelled)?;
                let path = lease.path().to_owned();
                out.leases.push(lease);
                leased.insert(key, path.clone());
                path
            }
        };
        sources[i] = Some(path);
    }
    for ((_, m), source) in caches.iter().zip(sources) {
        out.binds.push(BindMount {
            source: source.unwrap_or_default(),
            target: mount_target(&m.target, workdir),
            kind: MountKind::Directory,
            read_only: m.read_only,
        });
    }
    Ok(out)
}

/// Refuses the `RUN --mount` types and options this phase lacks, and the
/// malformed ones, in every mode, before any secret is read or cache
/// leased: `bind`, `tmpfs` and `ssh` (Phase 5) [open: Q-383],
/// `cache,from=`, a cache without a target, a secret without an id, an
/// `env=` that names no variable.
pub(crate) fn check_supported(mounts: &[Mount]) -> Result<()> {
    for m in mounts {
        match m.kind {
            MountType::Cache if !m.from.is_empty() => {
                return Err(Error::Invalid(
                    "RUN --mount=type=cache,from= is not supported yet (Phase 5)".to_owned(),
                ));
            }
            MountType::Cache if m.target.is_empty() => {
                return Err(Error::Invalid(
                    "RUN --mount=type=cache needs a target".to_owned(),
                ));
            }
            MountType::Cache => {}
            MountType::Secret => {
                if secret_id(m).is_empty() {
                    return Err(Error::Invalid(
                        "RUN --mount=type=secret needs an id, a source or a target".to_owned(),
                    ));
                }
                if let Some(name) = &m.env
                    && (name.is_empty() || name.contains(['=', '\0']))
                {
                    return Err(Error::Invalid(format!(
                        "RUN --mount=type=secret,env={name:?} is not a variable name"
                    )));
                }
            }
            MountType::Bind | MountType::Tmpfs | MountType::Ssh => {
                return Err(Error::Invalid(format!(
                    "RUN --mount=type={} is not supported yet (Phase 5)",
                    m.kind
                )));
            }
        }
    }
    Ok(())
}

/// The `RUN --mount`s a `Direct` `RUN` (kaniko mode on `/`, where nothing
/// can be mounted) runs with, checked before any secret is read
/// (ADR-0057): a secret only as a variable (`env=` without `target=`); a
/// secret as a file is refused, as it would need the `RUN`'s own tmpfs
/// (Q-380); a cache mount is left out with a warning, the `RUN` running
/// without the cache (Q-381). By design for now: kaniko mode on `/` keeps
/// nothing between builds, cache mounts included (ADR-0077); a safe way to
/// keep them is [open: Q-441].
pub(crate) fn direct_mounts(mounts: &[Mount]) -> Result<Vec<Mount>> {
    check_supported(mounts)?;
    let mut kept = Vec::with_capacity(mounts.len());
    for m in mounts {
        match m.kind {
            MountType::Cache => {
                tracing::warn!(
                    "RUN --mount=type=cache,target={}: ignored: kaniko mode on / runs RUN \
                     directly, where nothing can be mounted, and keeps no cache between \
                     builds (ADR-0077), so this RUN runs without the cache, and what it \
                     writes there lands in its layer (a tool's cache may hold tokens: clean \
                     it in the same RUN){}",
                    m.target,
                    if m.sharing == Some(Sharing::Locked) {
                        "; sharing=locked does not serialize it with other RUNs either"
                    } else {
                        ""
                    }
                );
            }
            MountType::Secret if m.env.is_none() || !m.target.is_empty() => {
                return Err(Error::Invalid(
                    "RUN --mount=type=secret as a file: kaniko mode on / runs RUN directly, \
                     where nothing can be mounted, and a secret is never written to the \
                     image's files; pass it as a variable with \
                     --mount=type=secret,id=<id>,env=<NAME> (no target=)"
                        .to_owned(),
                ));
            }
            // A secret as a variable: the only mount a `Direct` RUN keeps.
            MountType::Secret => kept.push(m.clone()),
            // Refused by `check_supported` above; refused here too, so that
            // relaxing that check can never drop a mount silently.
            MountType::Bind | MountType::Tmpfs | MountType::Ssh => {
                return Err(Error::Invalid(format!(
                    "RUN --mount=type={}: kaniko mode on / runs RUN directly, where nothing \
                     can be mounted",
                    m.kind
                )));
            }
        }
    }
    Ok(kept)
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
    match cache_owner(m) {
        None => id,
        Some(o) => format!("{id}\0uid={},gid={},mode={:o}", o.uid, o.gid, o.mode),
    }
}

/// A new cache's owner and mode when the mount sets any of them, BuildKit's
/// defaults filling the rest: uid and gid 0, mode `0755`. The store applies
/// them when it makes the cache, set-id bits dropped.
fn cache_owner(m: &Mount) -> Option<CacheOwner> {
    if m.uid.is_none() && m.gid.is_none() && m.mode.is_none() {
        return None;
    }
    Some(CacheOwner {
        uid: m.uid.unwrap_or(0),
        gid: m.gid.unwrap_or(0),
        mode: m.mode.unwrap_or(0o755),
    })
}

/// A secret mount's id: `id=`, else `source=`, else the target's last
/// name, as BuildKit; empty when none is given.
fn secret_id(m: &Mount) -> String {
    if !m.source.is_empty() {
        m.source.clone()
    } else if !m.cache_id.is_empty() {
        m.cache_id.clone()
    } else {
        Path::new(&m.target)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// A `RUN --mount=type=secret`, as BuildKit's `dispatchSecret` and secret
/// mount (`convert_secrets.go`, `secretmount_unix.go`):
///
/// - the id is `id=`, else `source=`, else the target's last name;
/// - a missing secret is an error when `required`, else the mount is left
///   out;
/// - with `env=`, the value is that variable of this `RUN` only;
/// - without `env=`, or with a `target=`, the value is a file at the
///   target, else at `/run/secrets/<id>`, mode `mode` (default `0400`,
///   permission bits only) and owner `uid`:`gid` (default root), which the
///   sandbox writes on a tmpfs of the `RUN`'s own and binds read-only.
///
/// The bytes are those [`Secret::load`] read at the start, one allocation
/// shared by every mount of the id, which the sandbox writes as one file.
fn secret_mount(
    m: &Mount,
    workdir: &Path,
    secrets: &BTreeMap<String, SecretValue>,
    out: &mut RunMounts,
) -> Result<()> {
    // Never empty: `check_supported` refused that.
    let id = secret_id(m);
    // A required one was checked by `run_mounts`: a missing one is left out.
    let Some(secret) = secrets.get(&id) else {
        return Ok(());
    };
    let data = Arc::clone(&secret.bytes);
    // A variable name: `check_supported` refused any other.
    if let Some(name) = &m.env {
        // Never the value in a message: only the id.
        if data.contains(&0) {
            return Err(Error::Invalid(format!(
                "secret {id:?} contains a NUL byte and cannot be a variable"
            )));
        }
        out.env
            .push((name.into(), OsString::from_vec(data.to_vec())));
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
    out.secrets.push(SecretFile {
        data,
        target,
        uid: m.uid.unwrap_or(0),
        gid: m.gid.unwrap_or(0),
        mode: m.mode.unwrap_or(0o400) & 0o777,
    });
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
