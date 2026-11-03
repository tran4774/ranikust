//! Context sources and the opened [`Context`].

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use rnk_ignore::Matcher;
use rnk_rootfs::{Fence, HostFile, RootPath};

use crate::spec::ContextSpec;
use crate::walk::Walk;
use crate::{Error, Result};

/// A source of build contexts. New kinds (tar, git, HTTPS) implement it.
///
/// Synchronous for now: Phase 1 has only local directories
/// ([open: #6]).
pub trait ContextSource {
    /// Makes the context available and opens it.
    ///
    /// # Errors
    ///
    /// Whatever prevents the source from being fetched or opened.
    fn open(&self) -> Result<Context>;
}

/// A local directory used in place, never modified.
#[derive(Clone, Debug)]
pub struct DirSource {
    path: PathBuf,
}

impl DirSource {
    /// A source for the directory at `path` on the host.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl ContextSource for DirSource {
    fn open(&self) -> Result<Context> {
        // The operator types the path, but the build context's owner may
        // control a symlink in it: one committed to the checkout
        // (`services/api -> /root/.docker`), or a component they own. So
        // none is followed (`RESOLVE_NO_SYMLINKS`, ADR-0076); `..` is then
        // the same physically and lexically.
        let dir_err = |source| Error::Dir {
            path: self.path.clone(),
            source,
        };
        let abs = std::path::absolute(&self.path).map_err(dir_err)?;
        let root = RootPath::open(&abs).map_err(|e| match e {
            rnk_rootfs::Error::Io { source, .. }
                if source.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                Error::Symlink {
                    path: self.path.clone(),
                }
            }
            rnk_rootfs::Error::Io { source, .. } => dir_err(source),
            e => e.into(),
        })?;
        let fence = Fence::of(&root)?;
        Ok(Context {
            root,
            origin: clean(&abs),
            fence,
        })
    }
}

/// `path` (absolute) with `.` dropped and `..` taken back, lexically: the
/// physical path once no component is a symlink.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Opens the source a parsed `--context` names.
///
/// # Errors
///
/// [`Error::Unsupported`] for tar, git and HTTPS contexts (Phase 5), or the
/// source's own error.
pub fn open(spec: &ContextSpec) -> Result<Context> {
    match spec {
        ContextSpec::Dir(path) => DirSource::new(path.clone()).open(),
        ContextSpec::Tar(_) => Err(Error::Unsupported {
            kind: "tar",
            reason: "tar contexts arrive in Phase 5",
        }),
        ContextSpec::Git(_) => Err(Error::Unsupported {
            kind: "git",
            reason: "git contexts arrive in Phase 5",
        }),
        ContextSpec::Https(_) => Err(Error::Unsupported {
            kind: "https",
            reason: "HTTPS contexts arrive in Phase 5",
        }),
    }
}

/// An opened build context: a read-only view for `COPY` and `ADD`.
///
/// Every path inside it is reached through its [`RootPath`], so `..` and
/// symlinks in the context cannot lead outside it.
#[derive(Debug)]
pub struct Context {
    root: RootPath,
    origin: PathBuf,
    /// The directory the context was opened from, which an output must not
    /// lead into, and its owner (ADR-0076).
    fence: Fence,
}

/// Where the Dockerfile was read from, for the ignore file beside it.
#[derive(Clone, Copy, Debug)]
pub enum DockerfileAt<'a> {
    /// A host file outside the context, the operator's own: the Dockerfile
    /// as `rnk_rootfs::reach` pinned it, with its directory.
    Host(&'a HostFile),
    /// A path inside the context, relative to its root: what its owner
    /// put there, reached through the context's root (ADR-0076).
    Context(&'a Path),
}

/// Where the ignore file was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IgnoreSource {
    /// `<Dockerfile>.dockerignore`, beside the Dockerfile (its host path,
    /// for messages).
    Dockerfile(PathBuf),
    /// `.dockerignore` at the root of the context.
    ContextRoot,
}

/// The contents of the ignore file in effect.
#[derive(Clone, Debug)]
pub struct IgnoreFile {
    /// Where it was found.
    pub source: IgnoreSource,
    /// The raw bytes, parsed by [`rnk_ignore::read_patterns`].
    pub contents: Vec<u8>,
}

impl Context {
    /// The context's root, for reading the files a walk reports.
    pub fn root(&self) -> &RootPath {
        &self.root
    }

    /// The host directory the context was opened from, for messages.
    pub fn origin(&self) -> &Path {
        &self.origin
    }

    /// The context's directory as a [`Fence`]: an operator's output path
    /// must not lead into it, since its owner controls every name in it
    /// (ADR-0076).
    pub fn fence(&self) -> &Fence {
        &self.fence
    }

    /// The owner of the context's directory: the principal whose files the
    /// context holds (ADR-0076).
    pub fn owner(&self) -> u32 {
        self.fence.owner()
    }

    /// The directory `sub` inside this context as a context of its own
    /// (`--context-sub-path`; which contexts the CLI applies it to: ADR-0032).
    /// `sub` cannot lead outside the context.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] when `sub` is missing or not a directory.
    pub fn sub_context(&self, sub: impl AsRef<Path>) -> Result<Context> {
        let sub = sub.as_ref();
        // The whole context stays its owner's: the fence is not moved.
        Ok(Context {
            root: self.root.sub_root(sub)?,
            origin: self.origin.join(sub),
            fence: self.fence,
        })
    }

    /// Finds the ignore file: `<dockerfile>.dockerignore` first, then
    /// `.dockerignore` at the context root, as kaniko
    /// (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1195-1204`) and BuildKit do.
    ///
    /// Beside a Dockerfile on the host (the operator's own path), a
    /// `<dockerfile>.dockerignore` that is a symlink or not a regular file
    /// is refused rather than followed. Beside a Dockerfile in the context,
    /// it is read through the context's root like `.dockerignore`, so a
    /// symlink there never leaves the context (ADR-0076).
    ///
    /// # Errors
    ///
    /// [`Error::IgnoreFile`] when the file beside the Dockerfile cannot be
    /// read, [`Error::Rootfs`] when an ignore file in the context exists but
    /// is not a readable regular file.
    pub fn find_dockerignore(
        &self,
        dockerfile: Option<DockerfileAt<'_>>,
    ) -> Result<Option<IgnoreFile>> {
        let beside = |p: &Path| {
            let mut beside = OsString::from(p.as_os_str());
            beside.push(".dockerignore");
            PathBuf::from(beside)
        };
        match dockerfile {
            Some(DockerfileAt::Host(dockerfile)) => {
                let beside = beside(dockerfile.path());
                if let Some(contents) = read_beside_dockerfile(dockerfile, &beside)? {
                    return Ok(Some(IgnoreFile {
                        source: IgnoreSource::Dockerfile(beside),
                        contents,
                    }));
                }
            }
            Some(DockerfileAt::Context(rel)) => {
                let rel = beside(rel);
                if let Some(contents) = self.read_ignore(&rel)? {
                    return Ok(Some(IgnoreFile {
                        source: IgnoreSource::Dockerfile(self.origin.join(&rel)),
                        contents,
                    }));
                }
            }
            None => {}
        }
        Ok(self
            .read_ignore(Path::new(".dockerignore"))?
            .map(|contents| IgnoreFile {
                source: IgnoreSource::ContextRoot,
                contents,
            }))
    }

    /// Opens the regular file `rel` of the context for reading, following
    /// symlinks inside the context only, and refusing a hardlink planted
    /// from outside or a file the owner of its directory could not read, as
    /// `COPY` refuses them ([`rnk_rootfs::CopyOpts::from_context`],
    /// ADR-0076).
    ///
    /// # Errors
    ///
    /// As [`RootPath::open_context_file`].
    pub fn open_file(&self, rel: impl AsRef<Path>) -> rnk_rootfs::Result<std::os::fd::OwnedFd> {
        self.root.open_context_file(rel)
    }

    /// The ignore file `rel` in the context, `None` when it is missing.
    fn read_ignore(&self, rel: &Path) -> Result<Option<Vec<u8>>> {
        match self.open_file(rel) {
            Ok(fd) => {
                let mut contents = Vec::new();
                File::from(fd)
                    .read_to_end(&mut contents)
                    .map_err(|source| Error::IgnoreFile {
                        path: self.origin.join(rel),
                        source,
                    })?;
                Ok(Some(contents))
            }
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NOENT.raw_os_error()) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Compiles an ignore file into a matcher.
    ///
    /// # Errors
    ///
    /// [`Error::Ignore`] for a malformed ignore file or pattern.
    pub fn matcher(&self, ignore: &IgnoreFile) -> Result<Matcher> {
        let span = tracing::info_span!("rnk_context.ignore", patterns = tracing::field::Empty,);
        let _entered = span.enter();
        let patterns = rnk_ignore::read_patterns(&ignore.contents)?;
        span.record("patterns", patterns.len());
        Ok(Matcher::new(&patterns)?)
    }

    /// Walks the context as BuildKit's filtered walk does: entries in
    /// order, each judged against `ignore` with its parent's match state,
    /// and excluded directories skipped unless an exception may re-include
    /// something below them (ADR-0027).
    pub fn walk<'c>(&'c self, ignore: Option<&'c Matcher>) -> Walk<'c> {
        Walk::new(&self.root, ignore)
    }
}

/// Reads `<Dockerfile>.dockerignore` without following a final symlink.
/// Reads `<Dockerfile>.dockerignore` beside a host Dockerfile, reached from
/// the Dockerfile's own pinned directory, without following it: a symlink,
/// a FIFO or a device node there is refused, never opened. `None` when it
/// does not exist.
fn read_beside_dockerfile(dockerfile: &HostFile, path: &Path) -> Result<Option<Vec<u8>>> {
    let err = |source| Error::IgnoreFile {
        path: path.to_owned(),
        source,
    };
    let name = path.file_name().unwrap_or_default();
    let fd = match dockerfile.open_beside(name) {
        Ok(Some(fd)) => fd,
        Ok(None) => return Ok(None),
        Err(e) => return Err(err(io::Error::other(e))),
    };
    let mut contents = Vec::new();
    File::from(fd).read_to_end(&mut contents).map_err(err)?;
    Ok(Some(contents))
}
