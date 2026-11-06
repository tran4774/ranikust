//! A context on disk and the key plan of a Dockerfile over it.

#![allow(dead_code, reason = "shared by two test binaries; each uses a part")]

use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use rnk_builder::{ContextFiles, KeyPlan, PlanImage, PlanInput, StageKeys};
use rnk_cache::{CacheKey, Encoding, TimePolicy};
use rnk_context::ContextSpec;
use rnk_dockerfile::GraphOptions;
use rnk_oci::{Compression, ContainerConfig, Digest, ImageConfig, Platform};
use rnk_sandbox::{Isolation, Network};

/// A build context in a temporary directory.
pub(crate) struct Ctx {
    _dir: tempfile::TempDir,
    pub(crate) path: PathBuf,
}

/// A context holding `files`, created in their order: a path ending in `/`
/// is a directory, content `->T` a symlink to `T`, `|fifo` a FIFO, `|sock`
/// a socket; anything else a regular file of mode `0644`.
pub(crate) fn context(files: &[(&str, &[u8])]) -> Ctx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ctx");
    fs::create_dir(&path).unwrap();
    let ctx = Ctx { _dir: dir, path };
    for (name, content) in files {
        ctx.put(name, content);
    }
    ctx
}

impl Ctx {
    pub(crate) fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Adds one entry, as [`context`] does.
    pub(crate) fn put(&self, name: &str, content: &[u8]) {
        let p = self.file(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        if name.ends_with('/') {
            fs::create_dir_all(&p).unwrap();
            return;
        }
        match content {
            b"|fifo" => {
                use rustix::fs::{CWD, FileType, Mode, mknodat};
                mknodat(CWD, &p, FileType::Fifo, Mode::from_raw_mode(0o644), 0).unwrap();
            }
            b"|sock" => {
                drop(std::os::unix::net::UnixListener::bind(&p).unwrap());
                self.chmod(name, 0o644);
            }
            _ => match content.strip_prefix(b"->") {
                Some(target) => {
                    std::os::unix::fs::symlink(std::str::from_utf8(target).unwrap(), &p).unwrap();
                }
                None => {
                    fs::write(&p, content).unwrap();
                    self.chmod(name, 0o644);
                }
            },
        }
    }

    pub(crate) fn chmod(&self, name: &str, mode: u32) {
        fs::set_permissions(self.file(name), fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Sets the mtime of every entry but symlinks to `secs`.
    pub(crate) fn touch_all(&self, secs: u64) {
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let mut todo = vec![self.path.clone()];
        while let Some(p) = todo.pop() {
            let meta = fs::symlink_metadata(&p).unwrap();
            if meta.is_dir() {
                for e in fs::read_dir(&p).unwrap() {
                    todo.push(e.unwrap().path());
                }
            }
            if meta.is_file() || meta.is_dir() {
                let f = fs::File::open(&p).unwrap();
                f.set_times(fs::FileTimes::new().set_modified(time))
                    .unwrap();
            }
        }
    }
}

/// How the plan is computed, besides the Dockerfile and the context.
#[derive(Clone, Debug)]
pub(crate) struct Opts {
    pub(crate) build_args: Vec<(&'static str, &'static str)>,
    pub(crate) isolation: Isolation,
    pub(crate) network: Network,
    pub(crate) allow_network_host: bool,
    /// `--reproducible`'s epoch.
    pub(crate) epoch: Option<i64>,
    pub(crate) threads: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            build_args: Vec::new(),
            isolation: Isolation::Full,
            network: Network::Host,
            allow_network_host: false,
            epoch: None,
            threads: 1,
        }
    }
}

/// The digest and config of the image `base` the Dockerfiles use.
fn base() -> (Digest, ImageConfig) {
    let config = ImageConfig {
        architecture: "amd64".to_owned(),
        os: "linux".to_owned(),
        config: Some(ContainerConfig {
            env: Some(vec!["PATH=/usr/bin:/bin".to_owned()]),
            user: Some("app".to_owned()),
            working_dir: Some("/srv".to_owned()),
            ..ContainerConfig::default()
        }),
        ..ImageConfig::default()
    };
    (Digest::from_sha256_bytes(&[7; 32]), config)
}

pub(crate) fn encoding(epoch: Option<i64>) -> Encoding {
    Encoding {
        compression: Compression::Gzip,
        level: 6,
        times: epoch.map_or(TimePolicy::Kept, |epoch| TimePolicy::Clamped { epoch }),
    }
}

/// The keys of every stage of `dockerfile`, by graph index.
pub(crate) fn keys(dockerfile: &str, ctx: &Path, opts: &Opts) -> Vec<Option<StageKeys>> {
    let (plan, stages) = plan(dockerfile, ctx, opts);
    (0..stages).map(|i| plan.stage(i).cloned()).collect()
}

/// The plan of `dockerfile` and the number of its stages.
pub(crate) fn plan(dockerfile: &str, ctx: &Path, opts: &Opts) -> (KeyPlan, usize) {
    let context = rnk_context::open(&ContextSpec::Dir(ctx.to_owned())).unwrap();
    let ignore = context.find_dockerignore(None).unwrap();
    let matcher = ignore.as_ref().map(|i| context.matcher(i).unwrap());
    let context_files = ContextFiles::load(&context, matcher.as_ref()).unwrap();
    let df = rnk_dockerfile::parse(dockerfile).unwrap();
    let graph = rnk_dockerfile::stage_graph(
        &df,
        &GraphOptions {
            build_args: opts
                .build_args
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..GraphOptions::default()
        },
    )
    .unwrap();
    let (digest, config) = base();
    let images = BTreeMap::from([(
        "base".to_owned(),
        PlanImage {
            digest: &digest,
            config: &config,
        },
    )]);
    let platform = Platform::new("linux", "amd64");
    let encoding = encoding(opts.epoch);
    let plan = KeyPlan::compute(&PlanInput {
        graph: &graph,
        stages: &df.stages,
        images: &images,
        context: &context,
        context_files: &context_files,
        platform: &platform,
        isolation: opts.isolation,
        network: opts.network,
        allow_network_host: opts.allow_network_host,
        encoding: &encoding,
        threads: NonZeroUsize::new(opts.threads).unwrap(),
    });
    (plan, graph.stages.len())
}

/// The final key of the last stage, which must have one.
#[track_caller]
pub(crate) fn last(dockerfile: &str, ctx: &Path, opts: &Opts) -> CacheKey {
    keys(dockerfile, ctx, opts)
        .last()
        .cloned()
        .flatten()
        .and_then(|s| s.final_key())
        .unwrap_or_else(|| panic!("no final key for {dockerfile:?} with {opts:?}"))
}
