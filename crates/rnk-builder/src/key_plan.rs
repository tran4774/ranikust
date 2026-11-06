//! The key plan: the cache key of every step of every stage the build runs,
//! computed before any stage starts and without executing anything
//! (ADR-0114), from the Dockerfile, the context walk and the base images'
//! manifests. The inputs are ADR-0113's, BuildKit's (ADR-0032):
//!
//! - every step: its instruction after expansion, as canonical JSON with
//!   its flags; the `ARG`s declared and the environment in effect; `USER`,
//!   `WORKDIR` and `SHELL`; its heredocs' bodies, as BuildKit expands them;
//! - a `RUN`: the network BuildKit keys (its `--network`, or `none` for one
//!   without it under the build's `--network=none`,
//!   `buildkit@cd8ee244 frontend/dockerfile/dockerfile2llb/convert.go:823`,
//!   `convert_runnetwork.go:11-24`), each secret mount's id, target, mode
//!   and owner, never its value, and each cache mount, marked dropped where
//!   a `Direct` `RUN` drops it (ADR-0057);
//! - a `COPY` or `ADD` from the context: each file it writes, as it writes
//!   it, read through the context's root as the copy reads it; a FIFO,
//!   socket or device by its metadata, never opened. An `ADD` of a local
//!   archive is one file, the archive, as BuildKit keys it;
//! - a `COPY --from`: the image's platform manifest digest, or the source
//!   stage's final key ([`StepKey::reads_stage`]).
//!
//! A step the plan cannot key as the build would run it (a feature the
//! build refuses, an expansion or a read that fails, a `--chown`
//! \[open: #168]) has no key, and neither has any later step of its stage
//! or a stage built on it or reading it: fail closed, a miss. The one
//! exception is an `ADD` of a local archive, keyed though the build refuses
//! it until Phase 5: its key is defined, and no build pushes a layer under
//! it before then. Which keys are looked up is the caller's choice.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io;
use std::num::NonZeroUsize;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use rnk_cache::{
    CacheKey, Content, ContentHasher, CopyFrom, Encoding, File, FileKind, KeyInputs,
    Mount as KeyMount, Network as KeyNetwork, Sharing as KeySharing,
};
use rnk_context::Context;
use rnk_dockerfile::{
    Add, ArgDecl, Base, CmdLine, Copy, Device, Healthcheck, InlineFile, Instruction, KeyValue,
    Located, Mount, MountType, Network as DfNetwork, Origin, ReferenceKind, Run, Security, Sharing,
    Source, SourceContent, Stage, StageGraph, StageNode, StageScope,
};
use rnk_oci::{ConfigEdit, DEFAULT_SHELL, Digest, ImageConfig, Platform};
use rnk_rootfs::{Chmod, FileType, RootPath, Stat};
use rnk_sandbox::{Isolation, Network};
use serde_json::{Value, json};
use tracing::field::Empty;

use crate::error::{Error, Result};
use crate::mounts;
use crate::sources::ContextFiles;

/// An image the build reads, as the plan keys it.
#[derive(Debug, Clone, Copy)]
pub struct PlanImage<'a> {
    /// The digest of its manifest for the build's platform: the chain root
    /// of a stage built on it, the input of a `COPY --from` reading it.
    pub digest: &'a Digest,
    /// Its config: a stage built on it starts from its environment,
    /// `USER`, `WORKDIR` and `SHELL`.
    pub config: &'a ImageConfig,
}

/// What the plan is computed from: what the build knows before any stage
/// runs.
pub struct PlanInput<'a> {
    /// The stage graph.
    pub graph: &'a StageGraph,
    /// The Dockerfile's stages, indexed as the graph indexes them.
    pub stages: &'a [Stage],
    /// The images of `FROM` and `COPY --from`, by reference as written.
    pub images: &'a BTreeMap<String, PlanImage<'a>>,
    /// The build context.
    pub context: &'a Context,
    /// The context's entries `.dockerignore` keeps.
    pub context_files: &'a ContextFiles,
    /// The image platform: the chain root of `FROM scratch`.
    pub platform: &'a Platform,
    /// How `RUN` is isolated: a `Direct` one drops cache mounts, and its
    /// `COPY` leaves special files out.
    pub isolation: Isolation,
    /// `--network`: the network of a `RUN` without `--network`.
    pub network: Network,
    /// `--allow network.host`: whether a `RUN --network=host` may lift
    /// `--network=none`.
    pub allow_network_host: bool,
    /// The layers' encoding, which tells whether mtimes are keyed.
    pub encoding: &'a Encoding,
    /// Threads hashing the context's files; the keys never depend on it.
    pub threads: NonZeroUsize,
}

/// One step's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepKey {
    /// The key.
    pub key: CacheKey,
    /// The stage a `COPY --from=<stage>` reads, whose final key is the
    /// step's input: the step is a miss when that stage runs in this build
    /// (ADR-0114).
    pub reads_stage: Option<usize>,
}

/// The keys of one stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageKeys {
    root: CacheKey,
    base_stage: Option<usize>,
    steps: Vec<Option<StepKey>>,
}

impl StageKeys {
    /// What the first step chains from: the base's platform manifest digest
    /// with the platform, the platform alone for `FROM scratch`, or the base
    /// stage's final key.
    #[must_use]
    pub fn root(&self) -> CacheKey {
        self.root
    }

    /// The stage this one is built on (`FROM <stage>`), whose final key is
    /// [`root`](Self::root): a miss there is a miss for every step here,
    /// or a cached layer would sit on a lower it was not made on (ADR-0114).
    #[must_use]
    pub fn base_stage(&self) -> Option<usize> {
        self.base_stage
    }

    /// One entry per instruction, in order; `None` from the first step the
    /// plan could not key.
    #[must_use]
    pub fn steps(&self) -> &[Option<StepKey>] {
        &self.steps
    }

    /// The key of the stage's last step, or its root when it has none;
    /// `None` when a step has no key.
    #[must_use]
    pub fn final_key(&self) -> Option<CacheKey> {
        match self.steps.last() {
            None => Some(self.root),
            Some(step) => step.map(|s| s.key),
        }
    }
}

/// Every step's key, for each stage the build runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyPlan {
    stages: BTreeMap<usize, StageKeys>,
}

impl KeyPlan {
    /// Computes the plan. Blocking: reads and hashes the context files the
    /// `COPY`s and `ADD`s name, on `input.threads` threads.
    #[must_use]
    pub fn compute(input: &PlanInput<'_>) -> Self {
        let span = tracing::debug_span!(
            "rnk_builder.key_plan",
            stages = Empty,
            steps = Empty,
            keyed = Empty,
        );
        let _entered = span.enter();
        let reader = Reader::new(input);
        let mut plan = Self::default();
        let mut ends: BTreeMap<usize, End> = BTreeMap::new();
        let (mut stages, mut steps, mut keyed) = (0_u64, 0_u64, 0_u64);
        for &index in &input.graph.needed {
            let (Some(node), Some(stage)) =
                (input.graph.stages.get(index), input.stages.get(index))
            else {
                continue;
            };
            stages += 1;
            steps = steps.saturating_add(len_u64(stage.instructions.len()));
            let start = match start(input, node, &ends) {
                Ok(start) => start,
                Err(why) => {
                    tracing::debug!("cache keys: {}: no keys: {why}", node.display_name);
                    continue;
                }
            };
            let (keys, end) = plan.stage_keys(input, &reader, node, stage, start);
            keyed = keyed.saturating_add(len_u64(keys.steps.iter().flatten().count()));
            if let Some(end) = end {
                ends.insert(index, end);
            }
            plan.stages.insert(index, keys);
        }
        span.record("stages", stages);
        span.record("steps", steps);
        span.record("keyed", keyed);
        plan
    }

    /// The keys of the stage at `index` in the graph; `None` for a stage the
    /// build does not run, or one with no chain root (a base it cannot key).
    #[must_use]
    pub fn stage(&self, index: usize) -> Option<&StageKeys> {
        self.stages.get(&index)
    }

    fn stage_keys(
        &self,
        input: &PlanInput<'_>,
        reader: &Reader<'_>,
        node: &StageNode,
        stage: &Stage,
        start: Start,
    ) -> (StageKeys, Option<End>) {
        let Start {
            root,
            base_stage,
            mut scope,
            mut config,
        } = start;
        let mut prev = root;
        let mut steps = Vec::with_capacity(stage.instructions.len());
        let mut keyed = true;
        for (index, instruction) in stage.instructions.iter().enumerate() {
            if !keyed {
                steps.push(None);
                continue;
            }
            let step = Step {
                input,
                reader,
                plan: self,
                node,
                index,
            };
            match step.key(instruction, &mut scope, &mut config, prev) {
                Ok(key) => {
                    prev = key.key;
                    steps.push(Some(key));
                }
                Err(e) => {
                    tracing::debug!(
                        "cache keys: {}: line {}: no key from here on: {e}",
                        node.display_name,
                        instruction.lines.start
                    );
                    keyed = false;
                    steps.push(None);
                }
            }
        }
        let end = keyed.then_some(End {
            key: prev,
            scope,
            config,
        });
        (
            StageKeys {
                root,
                base_stage,
                steps,
            },
            end,
        )
    }
}

fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// What a stage starts from.
struct Start {
    root: CacheKey,
    base_stage: Option<usize>,
    scope: StageScope,
    config: ImageConfig,
}

/// A fully keyed stage's end, for `FROM <stage>`.
struct End {
    key: CacheKey,
    scope: StageScope,
    config: ImageConfig,
}

/// A stage's chain root and the variables and config it starts with, as
/// `build_stage` starts it.
fn start(
    input: &PlanInput<'_>,
    node: &StageNode,
    ends: &BTreeMap<usize, End>,
) -> std::result::Result<Start, String> {
    if !node.triggers.is_empty() {
        return Err("ONBUILD triggers of a base stage are not supported yet".to_owned());
    }
    match &node.base {
        Base::Scratch => Ok(Start {
            root: CacheKey::root(None, input.platform),
            base_stage: None,
            scope: input.graph.scope_from_image::<&str>(&[]),
            config: ImageConfig {
                architecture: input.platform.architecture.clone(),
                os: input.platform.os.clone(),
                ..ImageConfig::default()
            },
        }),
        Base::Image(reference) => {
            let image = input
                .images
                .get(reference)
                .ok_or_else(|| format!("the image {reference} was not resolved"))?;
            let run = image.config.config.as_ref();
            if run
                .and_then(|c| c.on_build.as_ref())
                .is_some_and(|t| !t.is_empty())
            {
                return Err(format!(
                    "the image {reference} has ONBUILD triggers, which are not supported yet"
                ));
            }
            let env = run.and_then(|c| c.env.clone()).unwrap_or_default();
            Ok(Start {
                root: CacheKey::root(Some(image.digest), input.platform),
                base_stage: None,
                scope: input.graph.scope_from_image(&env),
                config: image.config.clone(),
            })
        }
        Base::Stage(base) => {
            let end = ends
                .get(base)
                .ok_or_else(|| "its base stage has no final key".to_owned())?;
            Ok(Start {
                root: end.key,
                base_stage: Some(*base),
                scope: end.scope.clone(),
                config: end.config.clone(),
            })
        }
    }
}

/// One instruction of a stage being keyed.
struct Step<'p, 'i> {
    input: &'p PlanInput<'i>,
    reader: &'p Reader<'i>,
    plan: &'p KeyPlan,
    node: &'p StageNode,
    index: usize,
}

impl Step<'_, '_> {
    /// The step's key; `scope` and `config` then hold its effect on the
    /// variables, `USER`, `WORKDIR` and `SHELL`.
    fn key(
        &self,
        instruction: &Located<Instruction>,
        scope: &mut StageScope,
        config: &mut ImageConfig,
        prev: CacheKey,
    ) -> Result<StepKey> {
        // Opened before any file is hashed, so `rnk_cache.key` times it.
        let mut inputs = KeyInputs::new(self.input.encoding);
        // What is in effect when the step runs, before its own change.
        let mut args: BTreeMap<&str, Option<&str>> = BTreeMap::new();
        for ArgDecl { key, value } in scope.args() {
            args.insert(key, value.as_deref());
        }
        for (name, value) in args {
            inputs.arg(name, value);
        }
        for (name, value) in scope.env().iter() {
            inputs.env(name, value);
        }
        let run = config.config.as_ref();
        let workdir = working_dir(config);
        let shell = run
            .and_then(|c| c.shell.clone())
            .unwrap_or_else(|| DEFAULT_SHELL.map(str::to_owned).to_vec());
        inputs
            .user(run.and_then(|c| c.user.as_deref()).unwrap_or_default())
            .workdir(&workdir.to_string_lossy())
            .shell(&shell);
        let expanded = scope.apply(instruction)?.value;
        let text = instruction_text(&expanded, &mut inputs);
        let mut reads_stage = None;
        match &expanded {
            Instruction::Run(r) => self.run(r, &workdir, &mut inputs)?,
            Instruction::Copy(c) => reads_stage = self.copy(c, &mut inputs)?,
            Instruction::Add(a) => self.add(a, &mut inputs)?,
            Instruction::User(user) => ConfigEdit::User(user.clone()).apply(config)?,
            Instruction::Workdir(dir) => ConfigEdit::Workdir(dir.clone()).apply(config)?,
            Instruction::Shell(shell) => ConfigEdit::Shell(shell.clone()).apply(config)?,
            Instruction::Env(_)
            | Instruction::Arg(_)
            | Instruction::Label(_)
            | Instruction::Expose(_)
            | Instruction::Volume(_)
            | Instruction::Entrypoint(_)
            | Instruction::Cmd(_)
            | Instruction::StopSignal(_)
            | Instruction::Healthcheck(_)
            | Instruction::Onbuild(_)
            | Instruction::Maintainer(_) => {}
        }
        Ok(StepKey {
            key: prev.next(&text, inputs),
            reads_stage,
        })
    }

    /// A `RUN`'s network and mounts, as the build runs it.
    fn run(&self, r: &Run, workdir: &Path, inputs: &mut KeyInputs) -> Result<()> {
        if r.security == Security::Insecure || !r.devices.is_empty() {
            return Err(unsupported("RUN --security=insecure or --device"));
        }
        let build_none = self.input.network == Network::None;
        let network = match r.network {
            DfNetwork::None => KeyNetwork::None,
            DfNetwork::Host if build_none && !self.input.allow_network_host => {
                return Err(unsupported(
                    "RUN --network=host under --network=none without --allow network.host",
                ));
            }
            DfNetwork::Host => KeyNetwork::Host,
            DfNetwork::Default if build_none => KeyNetwork::None,
            DfNetwork::Default => KeyNetwork::Default,
        };
        let direct = self.input.isolation == Isolation::Direct;
        if direct && network == KeyNetwork::None {
            return Err(unsupported("RUN --network=none in kaniko mode on /"));
        }
        mounts::check_supported(&r.mounts)?;
        inputs.network(network);
        for m in &r.mounts {
            let mount = match m.kind {
                MountType::Cache => KeyMount::Cache {
                    id: mounts::cache_id(m),
                    sharing: match m.sharing {
                        None | Some(Sharing::Shared) => KeySharing::Shared,
                        Some(Sharing::Private) => KeySharing::Private,
                        Some(Sharing::Locked) => KeySharing::Locked,
                    },
                    dropped: direct,
                },
                MountType::Secret => {
                    let file = m.env.is_none() || !m.target.is_empty();
                    if direct && file {
                        return Err(unsupported("a secret as a file in kaniko mode on /"));
                    }
                    let id = mounts::secret_id(m);
                    let target = if !file {
                        String::new()
                    } else if m.target.is_empty() {
                        let base = Path::new(&id)
                            .file_name()
                            .map_or_else(|| id.clone(), |n| n.to_string_lossy().into_owned());
                        format!("/run/secrets/{base}")
                    } else {
                        mounts::mount_target(&m.target, workdir)
                            .to_string_lossy()
                            .into_owned()
                    };
                    KeyMount::Secret {
                        id,
                        target,
                        mode: m.mode.unwrap_or(0o400) & 0o777,
                        uid: m.uid.unwrap_or(0),
                        gid: m.gid.unwrap_or(0),
                    }
                }
                // Refused by `check_supported` above.
                MountType::Bind | MountType::Tmpfs | MountType::Ssh => {
                    return Err(unsupported("this RUN --mount type"));
                }
            };
            inputs.mount(mount);
        }
        Ok(())
    }

    /// An `ADD`: a `COPY` from the context, its archives keyed as files.
    fn add(&self, a: &Add, inputs: &mut KeyInputs) -> Result<()> {
        unkeyed_chown(&a.chown)?;
        if !a.checksum.is_empty() || a.keep_git_dir.is_some() || a.unpack == Some(true) {
            return Err(unsupported("ADD --checksum, --keep-git-dir or --unpack"));
        }
        // `ADD <url>` gets its key with the feature (Phase 5).
        if a.sources.iter().any(|s| is_remote(s)) {
            return Err(unsupported("ADD of a URL or git repository"));
        }
        if a.link || !a.excludes.is_empty() {
            return Err(unsupported("ADD --link or --exclude"));
        }
        self.context_files(&a.sources, &a.chmod, inputs)
    }

    /// A `COPY`: the files it reads from the context, or what `--from`
    /// names; the stage it reads, if any.
    fn copy(&self, c: &Copy, inputs: &mut KeyInputs) -> Result<Option<usize>> {
        unkeyed_chown(&c.chown)?;
        if c.link || c.parents || !c.excludes.is_empty() {
            return Err(unsupported("COPY --link, --parents or --exclude"));
        }
        if c.from.is_empty() {
            self.context_files(&c.sources, &c.chmod, inputs)?;
            return Ok(None);
        }
        if c.sources.iter().any(String::is_empty) {
            return Err(unsupported("an empty source"));
        }
        parse_chmod(&c.chmod)?;
        let reference = self
            .node
            .references
            .iter()
            .find(|r| {
                r.origin == Origin::Instruction(self.index) && r.kind == ReferenceKind::CopyFrom
            })
            .ok_or_else(|| Error::Invalid("COPY --from was not resolved".to_owned()))?;
        match &reference.source {
            Source::Stage(s) => {
                let key = self
                    .plan
                    .stage(*s)
                    .and_then(StageKeys::final_key)
                    .ok_or_else(|| Error::Invalid(format!("stage {s} has no final key")))?;
                inputs.copy_from(CopyFrom::Stage(key));
                Ok(Some(*s))
            }
            Source::Image(i) => {
                let image = self
                    .input
                    .images
                    .get(i)
                    .ok_or_else(|| Error::Invalid(format!("the image {i} was not resolved")))?;
                inputs.copy_from(CopyFrom::Image(image.digest.clone()));
                Ok(None)
            }
        }
    }

    /// The context files `sources` name, as the copy writes them: owned by
    /// 0:0 (no `--chown`), `--chmod` applied, the special files a `Direct`
    /// build's copy leaves out left out, and a socket as the empty file the
    /// copy makes of it.
    fn context_files(&self, sources: &[String], chmod: &str, inputs: &mut KeyInputs) -> Result<()> {
        if sources.iter().any(String::is_empty) {
            return Err(unsupported("an empty source"));
        }
        let chmod = parse_chmod(chmod)?;
        let files = self.input.context_files;
        let mut jobs: Vec<Job> = Vec::new();
        for (group, source) in sources
            .iter()
            .map(|s| files.resolve(s))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .enumerate()
        {
            let named = source.named.into_os_string().into_vec();
            if source.dir {
                for path in files.below(&source.path) {
                    let rel = path.strip_prefix(&source.path).unwrap_or(path);
                    let mut name = PathBuf::from(OsString::from_vec(named.clone()));
                    name.push(rel);
                    jobs.push(Job {
                        name: name.into_os_string().into_vec(),
                        path: path.to_owned(),
                        group,
                    });
                }
            }
            jobs.push(Job {
                name: named,
                path: source.path,
                group,
            });
        }
        let described = self.reader.describe_all(&jobs)?;
        // `build_stage`'s `copy_skips_special`, for the one isolation known
        // to skip: keying an entry the copy leaves out only costs a miss.
        let skip_special = self.input.isolation == Isolation::Direct;
        // A copy links the later names of an inode to its first, within one
        // source, in the order it reads them (rnk-rootfs `Copier::node`).
        let mut first: HashMap<(usize, u32, u32, u64), Vec<u8>> = HashMap::new();
        let mut order: Vec<usize> = (0..jobs.len()).collect();
        order.sort_by(|&a, &b| (jobs[a].group, &jobs[a].path).cmp(&(jobs[b].group, &jobs[b].path)));
        for i in order {
            let (job, entry) = (&jobs[i], &described[i]);
            let st = &entry.stat;
            let kind = match &entry.kind {
                Raw::Fifo | Raw::Char(..) | Raw::Block(..) | Raw::Socket if skip_special => {
                    continue;
                }
                Raw::Regular(content) => FileKind::Regular(*content),
                Raw::Dir => FileKind::Directory,
                Raw::Symlink(target) => FileKind::Symlink(target.clone()),
                Raw::Fifo => FileKind::Fifo,
                Raw::Char(major, minor) => FileKind::CharDevice {
                    major: *major,
                    minor: *minor,
                },
                Raw::Block(major, minor) => FileKind::BlockDevice {
                    major: *major,
                    minor: *minor,
                },
                Raw::Socket => FileKind::Regular(ContentHasher::new().finish()),
            };
            let dir = matches!(entry.kind, Raw::Dir);
            let kind = if !dir && st.nlink > 1 {
                match first.entry((job.group, st.dev_major, st.dev_minor, st.ino)) {
                    std::collections::hash_map::Entry::Occupied(e) => {
                        FileKind::Hardlink(e.get().clone())
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(job.name.clone());
                        kind
                    }
                }
            } else {
                kind
            };
            let mode = match (&entry.kind, &chmod) {
                (Raw::Symlink(_), _) | (_, None) => st.permissions(),
                (
                    Raw::Regular(_)
                    | Raw::Dir
                    | Raw::Fifo
                    | Raw::Char(..)
                    | Raw::Block(..)
                    | Raw::Socket,
                    Some(chmod),
                ) => chmod.apply(st.permissions(), dir) & 0o7777,
            };
            inputs.file(File {
                path: job.name.clone(),
                kind,
                mode,
                uid: 0,
                gid: 0,
                capability: entry.capability.clone(),
                mtime: st.mtime.secs,
            });
        }
        Ok(())
    }
}

/// A `--chown` resolves in the stage's root, which does not exist before
/// the build: such a step gets no key until #168 says what stands for the
/// ids (ADR-0113 keys them).
fn unkeyed_chown(chown: &str) -> Result<()> {
    // [open: #168]
    if chown.is_empty() {
        Ok(())
    } else {
        Err(unsupported("--chown"))
    }
}

fn parse_chmod(chmod: &str) -> Result<Option<Chmod>> {
    if chmod.is_empty() {
        return Ok(None);
    }
    Ok(Some(Chmod::parse(chmod)?))
}

/// The sources `ADD` refuses as remote until Phase 5, as `build_stage`
/// tells them.
fn is_remote(source: &str) -> bool {
    source.starts_with("git@") || source.contains("://")
}

fn unsupported(what: &str) -> Error {
    Error::Invalid(format!("{what}: not supported, so not keyed"))
}

/// `WORKDIR` in effect, as a `RUN` gets it.
fn working_dir(config: &ImageConfig) -> PathBuf {
    let wd = config
        .config
        .as_ref()
        .and_then(|c| c.working_dir.as_deref())
        .unwrap_or("");
    Path::new("/").join(wd)
}

/// A context entry to describe: its name in the key, its path in the
/// context, and the source it is copied with.
struct Job {
    name: Vec<u8>,
    path: PathBuf,
    group: usize,
}

/// What a context entry is, before a step's `--chmod`.
#[derive(Clone)]
enum Raw {
    Regular(Content),
    Dir,
    Symlink(Vec<u8>),
    Fifo,
    Char(u32, u32),
    Block(u32, u32),
    Socket,
}

/// A described context entry.
#[derive(Clone)]
struct Described {
    stat: Stat,
    kind: Raw,
    capability: Option<Vec<u8>>,
}

/// Reads context entries for the keys, each once per plan.
struct Reader<'a> {
    root: &'a RootPath,
    threads: NonZeroUsize,
    seen: Mutex<HashMap<PathBuf, Described>>,
}

impl<'a> Reader<'a> {
    fn new(input: &PlanInput<'a>) -> Self {
        Self {
            root: input.context.root(),
            threads: input.threads,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Describes every job's entry, in the jobs' order, on up to
    /// `threads` threads; the first error stops the others.
    fn describe_all(&self, jobs: &[Job]) -> Result<Vec<Described>> {
        let workers = self.threads.get().min(jobs.len());
        if workers <= 1 {
            return jobs.iter().map(|j| self.describe(&j.path)).collect();
        }
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let mut out: Vec<Option<Described>> = vec![None; jobs.len()];
        let work = || -> Result<Vec<(usize, Described)>> {
            let mut mine = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(job) = jobs.get(i) else { break };
                match self.describe(&job.path) {
                    Ok(d) => mine.push((i, d)),
                    Err(e) => {
                        stop.store(true, Ordering::Relaxed);
                        return Err(e);
                    }
                }
            }
            Ok(mine)
        };
        let done: Vec<Result<Vec<(usize, Described)>>> = std::thread::scope(|s| {
            // This thread works too and a thread the system refuses is
            // skipped, so the jobs finish even with none spawned.
            let handles: Vec<_> = (1..workers)
                .filter_map(|_| std::thread::Builder::new().spawn_scoped(s, work).ok())
                .collect();
            let mut done = vec![work()];
            done.extend(
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))),
            );
            done
        });
        for part in done {
            for (i, d) in part? {
                out[i] = Some(d);
            }
        }
        out.into_iter()
            .map(|d| d.ok_or_else(|| Error::Invalid("a context entry was not read".to_owned())))
            .collect()
    }

    fn describe(&self, path: &Path) -> Result<Described> {
        let seen = |s: &Self| {
            s.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(path)
                .cloned()
        };
        if let Some(d) = seen(self) {
            return Ok(d);
        }
        let d = self.read(path)?;
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), d.clone());
        Ok(d)
    }

    /// Reads one entry as the copy reads it: by its physical path, pinned,
    /// a regular file through the context rule (ADR-0076) and checked to be
    /// the inode pinned; anything else by its metadata only.
    fn read(&self, path: &Path) -> Result<Described> {
        if path.as_os_str().is_empty() {
            let stat = self.root.statx(path)?;
            let capability = capability(self.root.xattrs(path)?);
            return Ok(Described {
                stat,
                kind: Raw::Dir,
                capability,
            });
        }
        let pinned = self.root.pin(path)?;
        let stat = *pinned.stat();
        let kind = match stat.file_type() {
            FileType::Regular => Raw::Regular(self.content(path, &stat)?),
            FileType::Directory => Raw::Dir,
            FileType::Symlink => Raw::Symlink(pinned.read_link()?.into_os_string().into_vec()),
            FileType::Fifo => Raw::Fifo,
            FileType::CharDevice => Raw::Char(stat.rdev_major, stat.rdev_minor),
            FileType::BlockDevice => Raw::Block(stat.rdev_major, stat.rdev_minor),
            FileType::Socket => Raw::Socket,
            FileType::Unknown => {
                return Err(Error::Invalid(format!(
                    "{}: an entry of an unknown type",
                    path.display()
                )));
            }
        };
        let capability = match kind {
            Raw::Symlink(_) => None,
            _ => capability(pinned.xattrs()?),
        };
        Ok(Described {
            stat,
            kind,
            capability,
        })
    }

    /// The content of the regular file `path`, which `stat` describes.
    fn content(&self, path: &Path, stat: &Stat) -> Result<Content> {
        let fd = self.root.open_context_file(path)?;
        let st = rustix::fs::fstat(&fd).map_err(|e| Error::io("stat", path)(e.into()))?;
        let same = st.st_ino == stat.ino
            && rustix::fs::major(st.st_dev) == stat.dev_major
            && rustix::fs::minor(st.st_dev) == stat.dev_minor;
        if !same {
            return Err(Error::Invalid(format!(
                "{}: changed while it was read",
                path.display()
            )));
        }
        let mut hasher = ContentHasher::new();
        io::copy(&mut std::fs::File::from(fd), &mut hasher)
            .map_err(|e| Error::io("read", path)(e))?;
        Ok(hasher.finish())
    }
}

fn capability(xattrs: Vec<(OsString, Vec<u8>)>) -> Option<Vec<u8>> {
    xattrs
        .into_iter()
        .find(|(name, _)| name.as_bytes() == b"security.capability")
        .map(|(_, value)| value)
}

/// The instruction's canonical text: JSON of every field after expansion,
/// with each heredoc body given to `inputs` instead. Every field is named,
/// so a new one does not compile until it is keyed.
fn instruction_text(i: &Instruction, inputs: &mut KeyInputs) -> String {
    let v = match i {
        Instruction::Run(r) => json!({ "run": run_json(r, inputs) }),
        Instruction::Copy(c) => json!({ "copy": copy_json(c, inputs) }),
        Instruction::Add(a) => json!({ "add": add_json(a, inputs) }),
        Instruction::Env(kvs) => json!({ "env": key_values(kvs) }),
        Instruction::Arg(decls) => {
            let decls: Vec<Value> = decls
                .iter()
                .map(|ArgDecl { key, value }| json!({ "key": key, "value": value }))
                .collect();
            json!({ "arg": decls })
        }
        Instruction::Label(kvs) => json!({ "label": key_values(kvs) }),
        Instruction::Workdir(dir) => json!({ "workdir": dir }),
        Instruction::User(user) => json!({ "user": user }),
        Instruction::Expose(ports) => json!({ "expose": ports }),
        Instruction::Volume(volumes) => json!({ "volume": volumes }),
        Instruction::Entrypoint(cmd) => json!({ "entrypoint": cmd_json(cmd, inputs) }),
        Instruction::Cmd(cmd) => json!({ "cmd": cmd_json(cmd, inputs) }),
        Instruction::Shell(shell) => json!({ "shell": shell }),
        Instruction::StopSignal(signal) => json!({ "stopsignal": signal }),
        Instruction::Healthcheck(h) => json!({ "healthcheck": health_json(h) }),
        Instruction::Onbuild(trigger) => json!({ "onbuild": trigger }),
        Instruction::Maintainer(name) => json!({ "maintainer": name }),
    };
    v.to_string()
}

fn key_values(kvs: &[KeyValue]) -> Vec<Value> {
    kvs.iter()
        .map(
            |KeyValue {
                 key,
                 value,
                 no_delim,
             }| { json!({ "key": key, "value": value, "no_delim": no_delim }) },
        )
        .collect()
}

/// A command line; a `RUN` heredoc's body goes to `inputs` as written, as
/// BuildKit expands none.
fn cmd_json(cmd: &CmdLine, inputs: &mut KeyInputs) -> Value {
    let CmdLine {
        args,
        prepend_shell,
        files,
    } = cmd;
    let files: Vec<Value> = files
        .iter()
        .map(|InlineFile { name, data, chomp }| {
            inputs.heredoc(name, data.as_bytes());
            json!({ "name": name, "chomp": chomp })
        })
        .collect();
    json!({ "args": args, "prepend_shell": prepend_shell, "files": files })
}

/// `COPY`'s and `ADD`'s heredoc sources; each body goes to `inputs` as
/// expanded.
fn contents_json(contents: &[SourceContent], inputs: &mut KeyInputs) -> Vec<Value> {
    contents
        .iter()
        .map(|SourceContent { path, data, expand }| {
            inputs.heredoc(path, data.as_bytes());
            json!({ "path": path, "expand": expand })
        })
        .collect()
}

fn run_json(r: &Run, inputs: &mut KeyInputs) -> Value {
    // The `--mount` values as written are keyed parsed and expanded, in
    // `mounts`.
    let Run {
        cmd,
        mount_specs: _,
        mounts,
        network,
        security,
        devices,
        flags_used,
    } = r;
    let mounts: Vec<Value> = mounts.iter().map(mount_json).collect();
    let devices: Vec<Value> = devices
        .iter()
        .map(|Device { name, required }| json!({ "name": name, "required": required }))
        .collect();
    json!({
        "cmd": cmd_json(cmd, inputs),
        "mounts": mounts,
        "network": network.as_str(),
        "security": security.as_str(),
        "devices": devices,
        "flags": flags_used,
    })
}

fn mount_json(m: &Mount) -> Value {
    let Mount {
        kind,
        from,
        source,
        target,
        read_only,
        size_limit,
        cache_id,
        sharing,
        required,
        env,
        mode,
        uid,
        gid,
    } = m;
    json!({
        "type": kind.as_str(),
        "from": from,
        "source": source,
        "target": target,
        "read_only": read_only,
        "size": size_limit,
        "id": cache_id,
        "sharing": sharing.map(Sharing::as_str),
        "required": required,
        "env": env,
        "mode": mode,
        "uid": uid,
        "gid": gid,
    })
}

fn copy_json(c: &Copy, inputs: &mut KeyInputs) -> Value {
    let Copy {
        sources,
        contents,
        dest,
        from,
        chown,
        chmod,
        link,
        parents,
        excludes,
    } = c;
    json!({
        "sources": sources,
        "contents": contents_json(contents, inputs),
        "dest": dest,
        "from": from,
        "chown": chown,
        "chmod": chmod,
        "link": link,
        "parents": parents,
        "excludes": excludes,
    })
}

fn add_json(a: &Add, inputs: &mut KeyInputs) -> Value {
    let Add {
        sources,
        contents,
        dest,
        chown,
        chmod,
        link,
        excludes,
        keep_git_dir,
        checksum,
        unpack,
    } = a;
    json!({
        "sources": sources,
        "contents": contents_json(contents, inputs),
        "dest": dest,
        "chown": chown,
        "chmod": chmod,
        "link": link,
        "excludes": excludes,
        "keep_git_dir": keep_git_dir,
        "checksum": checksum,
        "unpack": unpack,
    })
}

fn health_json(h: &Healthcheck) -> Value {
    let Healthcheck {
        test,
        interval,
        timeout,
        start_period,
        start_interval,
        retries,
    } = h;
    let nanos = |d: &std::time::Duration| d.as_nanos().to_string();
    json!({
        "test": test,
        "interval": nanos(interval),
        "timeout": nanos(timeout),
        "start_period": nanos(start_period),
        "start_interval": nanos(start_interval),
        "retries": retries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_sources_are_told_as_build_stage_tells_them() {
        assert!(is_remote("https://example.com/a.tgz"));
        assert!(is_remote("http://example.com/a"));
        assert!(is_remote("git@github.com:a/b.git"));
        assert!(is_remote("s3://bucket/key"));
        assert!(!is_remote("src/a.tgz"));
    }

    #[test]
    fn the_workdir_is_absolute() {
        let mut config = ImageConfig::default();
        assert_eq!(working_dir(&config), Path::new("/"));
        ConfigEdit::Workdir("app".to_owned())
            .apply(&mut config)
            .unwrap();
        assert_eq!(working_dir(&config), Path::new("/app"));
    }
}
