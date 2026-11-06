//! What a step's key covers besides its instruction and the previous key
//! (ADR-0113).

use std::fmt;
use std::io;

use rnk_oci::Digest;
use sha2::{Digest as _, Sha256};
use tracing::Span;

use crate::canonical::Canonical;
use crate::key::{CacheKey, Encoding, TimePolicy};

/// The inputs of one step's key: the build arguments, the environment,
/// `USER`, `WORKDIR` and `SHELL` in effect, and what the step reads.
///
/// Every input is a value the step's result depends on; there is no field
/// for the snapshot mode, a host user or id map, the build's time or a
/// thread count, and none meant for a secret's value. The order of
/// [`arg`](Self::arg), [`env`](Self::env) and [`file`](Self::file) calls
/// does not change the key; the order of [`mount`](Self::mount) calls
/// does, as the `RUN`'s does.
///
/// The `rnk_cache.key` span runs from [`KeyInputs::new`] to
/// [`CacheKey::next`]: create the inputs before hashing the files they hold.
/// `Debug` shows the names of build arguments and variables, never their
/// values.
pub struct KeyInputs {
    span: Span,
    times: TimePolicy,
    args: Vec<(String, Option<String>)>,
    env: Vec<(String, String)>,
    user: String,
    workdir: String,
    shell: Vec<String>,
    network: Option<Network>,
    mounts: Vec<Mount>,
    from: Option<CopyFrom>,
    files: Vec<File>,
}

impl KeyInputs {
    /// Empty inputs of a step whose layer is encoded as `encoding`, the
    /// one its tag names: its time policy decides whether a file adds its
    /// [`mtime`](File::mtime). No argument, environment, mount or file,
    /// and `USER`, `WORKDIR` and `SHELL` empty.
    #[must_use]
    pub fn new(encoding: &Encoding) -> Self {
        Self {
            span: tracing::debug_span!(
                "rnk_cache.key",
                files_hashed = tracing::field::Empty,
                bytes_hashed = tracing::field::Empty,
            ),
            times: encoding.times,
            args: Vec::new(),
            env: Vec::new(),
            user: String::new(),
            workdir: String::new(),
            shell: Vec::new(),
            network: None,
            mounts: Vec::new(),
            from: None,
            files: Vec::new(),
        }
    }

    /// A declared `ARG`: `None` when it has no value, which differs from an
    /// empty one (`${NAME-default}` tells them apart).
    pub fn arg(&mut self, name: &str, value: Option<&str>) -> &mut Self {
        self.args.push((name.to_owned(), value.map(str::to_owned)));
        self
    }

    /// A variable of the stage's environment in effect (`ENV` and the
    /// base's config), never one a secret mount's `env=` adds (threat model
    /// T11).
    pub fn env(&mut self, name: &str, value: &str) -> &mut Self {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    /// `USER` in effect, as written in the image config.
    pub fn user(&mut self, user: &str) -> &mut Self {
        user.clone_into(&mut self.user);
        self
    }

    /// `WORKDIR` in effect.
    pub fn workdir(&mut self, workdir: &str) -> &mut Self {
        workdir.clone_into(&mut self.workdir);
        self
    }

    /// `SHELL` in effect.
    pub fn shell<S: AsRef<str>>(&mut self, shell: &[S]) -> &mut Self {
        self.shell = shell.iter().map(|s| s.as_ref().to_owned()).collect();
        self
    }

    /// A `RUN`'s `--network`.
    pub fn network(&mut self, network: Network) -> &mut Self {
        self.network = Some(network);
        self
    }

    /// A `RUN`'s mount, in the `RUN`'s order.
    pub fn mount(&mut self, mount: Mount) -> &mut Self {
        self.mounts.push(mount);
        self
    }

    /// Where a `COPY --from` copies from.
    pub fn copy_from(&mut self, from: CopyFrom) -> &mut Self {
        self.from = Some(from);
        self
    }

    /// A file a `COPY` or `ADD` writes from the build context.
    pub fn file(&mut self, file: File) -> &mut Self {
        self.files.push(file);
        self
    }

    /// Feeds the inputs to `c` in their canonical form, and closes the span
    /// with what was hashed.
    pub(crate) fn feed(self, c: &mut Canonical) {
        let Self {
            span,
            times,
            mut args,
            mut env,
            user,
            workdir,
            shell,
            network,
            mounts,
            from,
            mut files,
        } = self;
        let epoch = match times {
            TimePolicy::Kept => None,
            TimePolicy::Clamped { epoch } => Some(epoch),
        };
        // Before sorting, so an mtime the key leaves out never orders files.
        for file in &mut files {
            file.mtime = epoch.map_or(0, |epoch| file.mtime.min(epoch));
        }
        // Unordered collections are sorted, duplicates kept, so the key is a
        // function of what was given and never of the order it came in.
        args.sort_unstable();
        env.sort_unstable();
        files.sort_unstable();

        c.len(args.len());
        for (name, value) in &args {
            c.str(name);
            c.opt_str(value.as_deref());
        }
        c.len(env.len());
        for (name, value) in &env {
            c.str(name);
            c.str(value);
        }
        c.str(&user);
        c.str(&workdir);
        c.strs(&shell);
        match network {
            None => c.u8(0),
            Some(network) => {
                c.u8(1);
                c.str(network.as_str());
            }
        }
        c.len(mounts.len());
        for mount in &mounts {
            mount.feed(c);
        }
        match &from {
            None => c.u8(0),
            Some(from) => {
                c.u8(1);
                from.feed(c);
            }
        }
        c.len(files.len());
        let (mut files_hashed, mut bytes_hashed) = (0_u64, 0_u64);
        for file in &files {
            file.feed(c, epoch.is_some());
            if let FileKind::Regular(content) = &file.kind {
                files_hashed += 1;
                bytes_hashed = bytes_hashed.saturating_add(content.size);
            }
        }
        span.record("files_hashed", files_hashed);
        span.record("bytes_hashed", bytes_hashed);
    }
}

impl fmt::Debug for KeyInputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn names<V>(pairs: &[(String, V)]) -> Vec<&str> {
            pairs.iter().map(|(name, _)| name.as_str()).collect()
        }
        f.debug_struct("KeyInputs")
            .field("times", &self.times)
            .field("args", &names(&self.args))
            .field("env", &names(&self.env))
            .field("user", &self.user)
            .field("workdir", &self.workdir)
            .field("shell", &self.shell)
            .field("network", &self.network)
            .field("mounts", &self.mounts)
            .field("from", &self.from)
            .field("files", &self.files.len())
            .finish_non_exhaustive()
    }
}

/// A `RUN`'s `--network`, as the Dockerfile gives it, never the network the
/// `RUN` ends up with in one mode or another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Network {
    /// `default`, also when the flag is absent.
    Default,
    /// `none`.
    None,
    /// `host`.
    Host,
}

impl Network {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::None => "none",
            Self::Host => "host",
        }
    }
}

/// `sharing=` of a cache mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sharing {
    /// `shared`, the default.
    Shared,
    /// `private`.
    Private,
    /// `locked`.
    Locked,
}

impl Sharing {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Private => "private",
            Self::Locked => "locked",
        }
    }
}

/// A `RUN --mount` the key names. The instruction text holds every flag;
/// these add the values defaults fill in, and whether a cache mount was
/// given to the `RUN` at all.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Mount {
    /// `type=secret`: what names and places the secret, never its value
    /// (threat model T11). Every secret mount the `RUN` names, whether the
    /// build is given the secret or not, as BuildKit's key
    /// (`buildkit@cd8ee244 solver/llbsolver/ops/exec.go:114-179`).
    ///
    /// A secret given only as an `env=` variable has no file: `target`
    /// empty, `mode`, `uid` and `gid` 0. BuildKit mounts nothing for it
    /// either, and keys its id and variable name
    /// (`buildkit@cd8ee244 client/llb/exec.go:435-443`), which the
    /// instruction holds.
    Secret {
        /// `id=`.
        id: String,
        /// Where the file is mounted.
        target: String,
        /// The file's mode.
        mode: u32,
        /// The file's owner.
        uid: u32,
        /// The file's group.
        gid: u32,
    },
    /// `type=cache`.
    Cache {
        /// `id=`.
        id: String,
        /// `sharing=`.
        sharing: Sharing,
        /// The mount was dropped rather than given to the `RUN`, as a
        /// `RUN` on `/` drops it (ADR-0057).
        dropped: bool,
    },
}

impl Mount {
    fn feed(&self, c: &mut Canonical) {
        match self {
            Self::Secret {
                id,
                target,
                mode,
                uid,
                gid,
            } => {
                c.str("secret");
                c.str(id);
                c.str(target);
                c.u32(*mode);
                c.u32(*uid);
                c.u32(*gid);
            }
            Self::Cache {
                id,
                sharing,
                dropped,
            } => {
                c.str("cache");
                c.str(id);
                c.str(sharing.as_str());
                c.u8(u8::from(*dropped));
            }
        }
    }
}

/// Where a `COPY --from` copies from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CopyFrom {
    /// An image: its platform manifest digest, which stands for the files
    /// it holds, so the step adds no [`File`] (ADR-0113).
    Image(Digest),
    /// A stage: its final key, which stands for the files it holds, so the
    /// step adds no [`File`] (ADR-0114).
    Stage(CacheKey),
}

impl CopyFrom {
    fn feed(&self, c: &mut Canonical) {
        match self {
            Self::Image(digest) => {
                c.str("image");
                c.str(digest.as_str());
            }
            Self::Stage(key) => {
                c.str("stage");
                c.u32(key.version());
                c.hash(key.hash());
            }
        }
    }
}

/// A file a `COPY` or `ADD` writes from the build context, as it writes it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct File {
    /// The file's path relative to the build context, with no host or
    /// mode-specific prefix: `dir1/x` and `dir2/x` stay apart. One path
    /// names one file: two files of the step never share one.
    pub path: Vec<u8>,
    /// Its type, with its content or link target.
    pub kind: FileKind,
    /// Permission bits with setuid, setgid and sticky, after `--chmod`.
    pub mode: u32,
    /// Owner after `--chown`.
    pub uid: u32,
    /// Group after `--chown`.
    pub gid: u32,
    /// The raw `security.capability` extended attribute.
    pub capability: Option<Vec<u8>>,
    /// Its mtime in seconds since the Unix epoch, before any clamp. The key
    /// holds it only under [`TimePolicy::Clamped`], clamped to the epoch as
    /// the layer's header carries it (ADR-0113).
    pub mtime: i64,
}

impl File {
    /// `mtime` is already the one the header carries when `clamped`.
    fn feed(&self, c: &mut Canonical, clamped: bool) {
        // Destructured so that a field added to `File` cannot be left out.
        let Self {
            path,
            kind,
            mode,
            uid,
            gid,
            capability,
            mtime,
        } = self;
        c.bytes(path);
        match kind {
            FileKind::Regular(content) => {
                c.str("regular");
                c.hash(&content.sha256);
                c.u64(content.size);
            }
            FileKind::Directory => c.str("directory"),
            FileKind::Symlink(target) => {
                c.str("symlink");
                c.bytes(target);
            }
            FileKind::Hardlink(target) => {
                c.str("hardlink");
                c.bytes(target);
            }
            FileKind::Fifo => c.str("fifo"),
            FileKind::CharDevice { major, minor } => {
                c.str("char");
                c.u32(*major);
                c.u32(*minor);
            }
            FileKind::BlockDevice { major, minor } => {
                c.str("block");
                c.u32(*major);
                c.u32(*minor);
            }
        }
        c.u32(*mode);
        c.u32(*uid);
        c.u32(*gid);
        c.opt_bytes(capability.as_deref());
        if clamped {
            c.u8(1);
            c.i64(*mtime);
        } else {
            c.u8(0);
        }
    }
}

/// The type of a [`File`], with what its type carries.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileKind {
    /// A regular file, with its content.
    Regular(Content),
    /// A directory.
    Directory,
    /// A symbolic link, with its target.
    Symlink(Vec<u8>),
    /// A hard link, with the path it links to.
    Hardlink(Vec<u8>),
    /// A named pipe.
    Fifo,
    /// A character device.
    CharDevice {
        /// Its major number.
        major: u32,
        /// Its minor number.
        minor: u32,
    },
    /// A block device.
    BlockDevice {
        /// Its major number.
        major: u32,
        /// Its minor number.
        minor: u32,
    },
}

/// The SHA-256 and size of a file's content, from a [`ContentHasher`].
///
/// Equal values mean the same bytes: the caller compares the bytes it
/// copied with the ones a key named.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Content {
    sha256: [u8; 32],
    size: u64,
}

impl Content {
    /// The SHA-256 of the content.
    #[must_use]
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// The content's size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Content")
            .field("sha256", &Digest::from_sha256_bytes(&self.sha256).as_str())
            .field("size", &self.size)
            .finish()
    }
}

/// Hashes a file's content as it is read, in chunks of any size.
#[derive(Clone, Default)]
pub struct ContentHasher {
    sha: Sha256,
    size: u64,
}

impl ContentHasher {
    /// A hasher that has seen no bytes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Hashes the next bytes of the content.
    pub fn update(&mut self, chunk: &[u8]) {
        self.sha.update(chunk);
        // The conversion cannot fail: `canonical` asserts usize fits in u64.
        let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        self.size = self.size.saturating_add(len);
    }

    /// The content seen so far.
    #[must_use]
    pub fn finish(self) -> Content {
        Content {
            sha256: self.sha.finalize().into(),
            size: self.size,
        }
    }
}

impl fmt::Debug for ContentHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContentHasher")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl io::Write for ContentHasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
