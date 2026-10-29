//! Context sources and the opened [`Context`].

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use rnk_ignore::Matcher;
use rnk_rootfs::RootPath;

use crate::spec::ContextSpec;
use crate::walk::Walk;
use crate::{Error, Result};

/// A source of build contexts. New kinds (tar, git, HTTPS) implement it.
///
/// Synchronous for now: Phase 1 has only local directories
/// ([open: Q-027]).
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
        // The user's own argument: resolving its symlinks on the host is
        // fine, and `RootPath::open` wants a path without any.
        let canonical = std::fs::canonicalize(&self.path).map_err(|source| Error::Dir {
            path: self.path.clone(),
            source,
        })?;
        let root = RootPath::open(&canonical)?;
        Ok(Context {
            root,
            origin: canonical,
        })
    }
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
}

/// Where the ignore file was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IgnoreSource {
    /// `<Dockerfile>.dockerignore`, beside the Dockerfile on the host.
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

    /// The directory `sub` inside this context as a context of its own
    /// (`--context-sub-path`). `sub` cannot lead outside the context.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] when `sub` is missing or not a directory.
    pub fn sub_context(&self, sub: impl AsRef<Path>) -> Result<Context> {
        let sub = sub.as_ref();
        Ok(Context {
            root: self.root.sub_root(sub)?,
            origin: self.origin.join(sub),
        })
    }

    /// Finds the ignore file: `<dockerfile>.dockerignore` first, then
    /// `.dockerignore` at the context root, as kaniko
    /// (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1195-1204`) and BuildKit do.
    ///
    /// `dockerfile` is the host path of the Dockerfile, the user's own
    /// argument. A `<dockerfile>.dockerignore` that is a symlink or not a
    /// regular file is refused rather than followed.
    ///
    /// # Errors
    ///
    /// [`Error::IgnoreFile`] when the file beside the Dockerfile cannot be
    /// read, [`Error::Rootfs`] when the context's `.dockerignore` exists but
    /// is not a readable regular file.
    pub fn find_dockerignore(&self, dockerfile: Option<&Path>) -> Result<Option<IgnoreFile>> {
        if let Some(dockerfile) = dockerfile {
            let mut beside = OsString::from(dockerfile.as_os_str());
            beside.push(".dockerignore");
            let beside = PathBuf::from(beside);
            if let Some(contents) = read_beside_dockerfile(&beside)? {
                return Ok(Some(IgnoreFile {
                    source: IgnoreSource::Dockerfile(beside),
                    contents,
                }));
            }
        }
        match self.root.open_regular(".dockerignore") {
            Ok(fd) => {
                let mut contents = Vec::new();
                File::from(fd)
                    .read_to_end(&mut contents)
                    .map_err(|source| Error::IgnoreFile {
                        path: self.origin.join(".dockerignore"),
                        source,
                    })?;
                Ok(Some(IgnoreFile {
                    source: IgnoreSource::ContextRoot,
                    contents,
                }))
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
/// `None` when it does not exist.
fn read_beside_dockerfile(path: &Path) -> Result<Option<Vec<u8>>> {
    let err = |source| Error::IgnoreFile {
        path: path.to_owned(),
        source,
    };
    // `O_NOFOLLOW` refuses a final symlink; `O_NONBLOCK` keeps a FIFO from
    // blocking the open, and the type check below refuses it.
    let flags = (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits();
    let flags = i32::try_from(flags).map_err(|_| {
        err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "open flags out of range",
        ))
    })?;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(err(e)),
    };
    if !file.metadata().map_err(err)?.file_type().is_file() {
        return Err(err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        )));
    }
    let mut contents = Vec::new();
    (&file).read_to_end(&mut contents).map_err(err)?;
    Ok(Some(contents))
}
