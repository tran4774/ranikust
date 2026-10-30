//! Building one stage: its base, then one executor per instruction.
//!
//! Instructions that change files (`RUN`, `COPY`, `ADD`, a `WORKDIR` that
//! creates its directory) each run in a layer of their own
//! (`StageRootfs::begin_layer`/`end_layer`); the others only edit the
//! config (`rnk_oci::ConfigEdit`) and mount nothing. A closed layer is
//! encoded at once when the stage's [`LayerDemand`] needs it, else
//! dropped: lazy export.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rnk_context::Context;
use rnk_dockerfile::{
    Add, CmdLine, Copy, Healthcheck, Instruction, Located, MountType, Network as DfNetwork, Origin,
    ReferenceKind, Security, Source, SourceContent, Stage, StageGraph, StageScope,
    chomp_heredoc_content, parse_heredoc_marker,
};
use rnk_oci::{CommandLine, ConfigEdit, HealthConfig, ImageConfig, ImageFormat, Platform};
use rnk_rootfs::{CopyOpts, Meta, OpenHow, Owner, RootPath};
use rnk_sandbox::{BindMount, MountKind, Network, RunSpec, Sandbox};
use rnk_snapshot::{Base, LayerDiff, Snapshotter, StageRootfs};
use rnk_store::Store;
use tracing::{debug_span, info, info_span};

use crate::error::{Error, Result};
use crate::image::layer_history;
use crate::layer::{BlobLayer, LayerDemand, LayerExporter, LayerSource};
use crate::pull::PulledImage;
use crate::sources::{self, ContextFiles};
use crate::time::BuildTime;

/// Hands out `--jobs` permits for `RUN` (ADR-0013): the scheduler's, seen
/// from the blocking thread a stage runs on.
pub trait RunGate: Sync {
    /// Waits for a permit and returns it; the permit is held until the
    /// returned value is dropped. `None` once the build is cancelled.
    fn enter(&self) -> Option<Box<dyn Send>>;
}

/// A gate that never waits: for tests and single `RUN`s.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ungated;

impl RunGate for Ungated {
    fn enter(&self) -> Option<Box<dyn Send>> {
        Some(Box::new(()))
    }
}

/// What every stage of a build shares.
pub struct BuildCx<'a, S> {
    /// Makes stage roots.
    pub snapshotter: &'a S,
    /// Runs `RUN`.
    pub sandbox: &'a Sandbox,
    /// Cache mount directories.
    pub store: &'a Store,
    /// The build context.
    pub context: &'a Context,
    /// The context's entries `.dockerignore` keeps.
    pub context_files: &'a ContextFiles,
    /// Encodes the layers that are needed.
    pub exporter: &'a LayerExporter,
    /// The build's clock.
    pub time: &'a BuildTime,
    /// The network of a `RUN` without `--network` (ADR-0010).
    pub network: Network,
    /// `RUN --mount=type=secret` sources: id to host file, absolute.
    // [open: Q-022] interim: kaniko's `--secret id=ID,src=PATH` for files.
    pub secrets: &'a BTreeMap<String, PathBuf>,
    /// The image platform, for `FROM scratch`.
    pub platform: &'a Platform,
    /// `--jobs` permits for `RUN`.
    pub run_gate: &'a dyn RunGate,
    /// Whether the build was cancelled; checked between instructions.
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
}

/// A root that `COPY --from` reads and `FROM <stage>` builds on, released
/// once by its owner.
#[derive(Debug)]
pub struct SourceRoot<R>(Mutex<Option<R>>);

impl<R: StageRootfs> SourceRoot<R> {
    /// Wraps a root.
    pub fn new(root: R) -> Self {
        Self(Mutex::new(Some(root)))
    }

    fn lock(&self) -> MutexGuard<'_, Option<R>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Unmounts and deletes the root; a second call does nothing. Every
    /// stage built on it must be released first.
    ///
    /// # Errors
    ///
    /// As `StageRootfs::release`; the root stays held for a retry.
    pub fn release(&self) -> Result<()> {
        let mut guard = self.lock();
        if let Some(root) = guard.take() {
            root.release()?;
        }
        Ok(())
    }
}

/// An image read by `COPY --from=<image>`: pulled, with a root on it, made
/// by the stage that reads it (preparation touches no root).
#[derive(Debug)]
pub struct ImageSource<R> {
    /// The pulled image.
    pub image: std::sync::Arc<PulledImage>,
    /// A root on its layers.
    pub root: SourceRoot<R>,
}

impl<R: StageRootfs> ImageSource<R> {
    /// A root on `image`'s layers.
    ///
    /// # Errors
    ///
    /// As `Snapshotter::prepare`.
    pub fn new<S: Snapshotter<Root = R>>(
        snapshotter: &S,
        image: std::sync::Arc<PulledImage>,
    ) -> Result<Self> {
        let root = snapshotter.prepare(Base::Image(&image.layers))?;
        Ok(Self {
            image,
            root: SourceRoot::new(root),
        })
    }
}

/// What a stage starts from.
pub enum StageBase<'a, R> {
    /// `FROM scratch`.
    Scratch,
    /// `FROM <image>`.
    Image(&'a PulledImage),
    /// `FROM <stage>`.
    Stage(&'a StageDone<R>),
}

/// One stage to build.
pub struct StageInput<'a, R> {
    /// The stage's index in the graph.
    pub index: usize,
    /// The stage graph.
    pub graph: &'a StageGraph,
    /// The stage's instructions, as parsed.
    pub stage: &'a Stage,
    /// Its base.
    pub base: StageBase<'a, R>,
    /// Finished stages it reads with `COPY --from`, by index.
    pub stages: BTreeMap<usize, &'a StageDone<R>>,
    /// Images it reads with `COPY --from`, by reference as written.
    pub images: BTreeMap<String, &'a ImageSource<R>>,
    /// Which of its layers are encoded.
    pub demand: LayerDemand,
}

/// A built stage.
#[derive(Debug)]
pub struct StageDone<R> {
    /// The stage's final root.
    pub root: SourceRoot<R>,
    /// The variables at its end, for `FROM <stage>`.
    pub scope: StageScope,
    /// Its image config.
    pub config: ImageConfig,
    /// Its layers, base first: complete only when its demand needed them.
    pub layers: Vec<BlobLayer>,
    /// Whether `layers` holds every layer (the demand needed them).
    pub complete: bool,
    /// The format of the image it is based on, if any.
    pub base_format: Option<ImageFormat>,
}

/// Builds one stage. On failure the stage's root is released before the
/// error returns.
///
/// Blocking: runs `RUN`, copies and layer encoding on the calling thread.
///
/// # Errors
///
/// [`Error::Instruction`] naming the instruction that failed,
/// [`Error::Cancelled`], or the error of preparing the base.
pub fn build_stage<S: Snapshotter>(
    cx: &BuildCx<'_, S>,
    input: StageInput<'_, S::Root>,
) -> Result<StageDone<S::Root>> {
    let node = &input.graph.stages[input.index];
    let span = info_span!(
        "rnk_builder.stage",
        stage = node.display_name.as_str(),
        instructions = input.stage.instructions.len(),
        layers = tracing::field::Empty,
    );
    let _entered = span.enter();
    if !node.triggers.is_empty() {
        return Err(Error::Invalid(format!(
            "{}: ONBUILD triggers of a base stage are not supported yet (Phase 5)",
            node.display_name
        )));
    }
    let (root, scope, config, layers, base_format) = match &input.base {
        StageBase::Scratch => {
            // kaniko gives `FROM scratch` a `PATH` (`ScratchEnvVars`,
            // `kaniko@44aa7ddc0 pkg/constants/constants.go:62`); an image
            // without one keeps none [open: Q-185].
            let config = ImageConfig {
                architecture: cx.platform.architecture.clone(),
                os: cx.platform.os.clone(),
                config: Some(rnk_oci::ContainerConfig {
                    env: Some(vec![format!("PATH={}", rnk_dockerfile::DEFAULT_PATH)]),
                    ..rnk_oci::ContainerConfig::default()
                }),
                ..ImageConfig::default()
            };
            let scope = input.graph.scope_from_image::<&str>(&[]);
            (
                cx.snapshotter.prepare(Base::Scratch)?,
                scope,
                config,
                Vec::new(),
                None,
            )
        }
        StageBase::Image(image) => {
            let env = image
                .config
                .config
                .as_ref()
                .and_then(|c| c.env.clone())
                .unwrap_or_default();
            if image
                .config
                .config
                .as_ref()
                .and_then(|c| c.on_build.as_ref())
                .is_some_and(|t| !t.is_empty())
            {
                return Err(Error::Invalid(format!(
                    "{}: the image {} has ONBUILD triggers, which are not supported yet \
                     (Phase 5)",
                    node.display_name, image.reference
                )));
            }
            let root = cx.snapshotter.prepare(Base::Image(&image.layers))?;
            (
                root,
                input.graph.scope_from_image(&env),
                image.config.clone(),
                image.blobs.clone(),
                Some(image.format),
            )
        }
        StageBase::Stage(parent) => {
            let guard = parent.root.lock();
            let parent_root = guard
                .as_ref()
                .ok_or_else(|| Error::Invalid("the base stage was released".to_owned()))?;
            let root = cx.snapshotter.prepare(Base::Stage(parent_root))?;
            (
                root,
                parent.scope.clone(),
                parent.config.clone(),
                parent.layers.clone(),
                parent.base_format,
            )
        }
    };
    let mut b = Builder {
        cx,
        input: &input,
        name: node.display_name.clone(),
        root,
        scope,
        config,
        layers,
        new_layers: 0,
        scripts: Vec::new(),
    };
    let result = b.run();
    span.record("layers", b.new_layers);
    match result {
        Ok(()) => Ok(StageDone {
            root: SourceRoot::new(b.root),
            scope: b.scope,
            config: b.config,
            layers: b.layers,
            complete: input.demand.needed(),
            base_format,
        }),
        Err(e) => {
            if let Err(r) = b.root.release() {
                tracing::warn!("releasing the root of {} after a failure: {r}", b.name);
            }
            Err(e)
        }
    }
}

struct Builder<'c, 'i, S: Snapshotter> {
    cx: &'c BuildCx<'c, S>,
    input: &'i StageInput<'i, S::Root>,
    name: String,
    root: S::Root,
    scope: StageScope,
    config: ImageConfig,
    layers: Vec<BlobLayer>,
    new_layers: u64,
    /// Scripts of `#!` heredocs, removed when the stage ends.
    scripts: Vec<tempfile::NamedTempFile>,
}

impl<S: Snapshotter> Builder<'_, '_, S> {
    fn run(&mut self) -> Result<()> {
        for (index, instruction) in self.input.stage.instructions.iter().enumerate() {
            if (self.cx.cancelled)() {
                return Err(Error::Cancelled);
            }
            let kind = instruction.value.name().to_ascii_uppercase();
            let span = info_span!(
                "rnk_builder.instruction",
                stage = self.name.as_str(),
                index,
                line = instruction.lines.start,
                kind = kind.as_str(),
                text = instruction.code.as_str(),
                cache_hit = false,
            );
            let _entered = span.enter();
            info!("[{}] {}", self.name, instruction.code);
            self.execute(index, instruction).map_err(|e| {
                // A cancel is reported as such, not as the instruction's
                // failure; every other error names the instruction.
                if matches!(e, Error::Cancelled) {
                    e
                } else {
                    Error::Instruction {
                        stage: self.name.clone(),
                        line: instruction.lines.start,
                        instruction: shorten(&instruction.code),
                        source: Box::new(e),
                    }
                }
            })?;
        }
        Ok(())
    }

    fn execute(&mut self, index: usize, instruction: &Located<Instruction>) -> Result<()> {
        let expanded = self.scope.apply(instruction)?;
        let _span = debug_span!("rnk_builder.execute").entered();
        let code = instruction.code.as_str();
        match expanded.value {
            Instruction::Arg(_) => Ok(()),
            Instruction::Env(kvs) => self.edit(ConfigEdit::Env(
                kvs.into_iter().map(|kv| (kv.key, kv.value)).collect(),
            )),
            Instruction::Label(kvs) => self.edit(ConfigEdit::Label(
                kvs.into_iter().map(|kv| (kv.key, kv.value)).collect(),
            )),
            Instruction::User(user) => self.edit(ConfigEdit::User(user)),
            Instruction::Expose(ports) => self.edit(ConfigEdit::Expose(ports)),
            // Config only, as Docker and BuildKit: the directories are not
            // created [open: Q-180].
            Instruction::Volume(volumes) => self.edit(ConfigEdit::Volume(volumes)),
            Instruction::Entrypoint(cmd) => self.edit(ConfigEdit::Entrypoint(command_line(cmd))),
            Instruction::Cmd(cmd) => self.edit(ConfigEdit::Cmd(command_line(cmd))),
            Instruction::Shell(shell) => self.edit(ConfigEdit::Shell(shell)),
            Instruction::StopSignal(signal) => self.edit(ConfigEdit::StopSignal(signal)),
            Instruction::Healthcheck(h) => self.edit(ConfigEdit::Healthcheck(health(&h))),
            Instruction::Onbuild(trigger) => self.edit(ConfigEdit::OnBuild(trigger)),
            Instruction::Maintainer(name) => {
                self.config.author = Some(name);
                Ok(())
            }
            Instruction::Workdir(dir) => self.workdir(dir, code),
            Instruction::Run(run) => self.run_command(run, code),
            Instruction::Copy(c) => self.copy(index, c, code),
            Instruction::Add(a) => self.add(index, a, code),
        }
    }

    fn edit(&mut self, edit: ConfigEdit) -> Result<()> {
        edit.apply(&mut self.config)?;
        Ok(())
    }

    fn working_dir(&self) -> PathBuf {
        let wd = self
            .config
            .config
            .as_ref()
            .and_then(|c| c.working_dir.as_deref())
            .unwrap_or("");
        if wd.is_empty() {
            PathBuf::from("/")
        } else {
            Path::new("/").join(wd)
        }
    }

    fn user(&self) -> String {
        self.config
            .config
            .as_ref()
            .and_then(|c| c.user.clone())
            .unwrap_or_default()
    }

    /// Runs `f` in a new layer, which is then encoded if needed.
    fn layer(
        &mut self,
        created_by: &str,
        f: impl FnOnce(&Self, &RootPath) -> Result<()>,
    ) -> Result<()> {
        self.root.begin_layer()?;
        let result = match self.root.root() {
            Ok(root) => f(self, root),
            Err(e) => Err(e.into()),
        };
        // Closed whatever happened: the diff is discarded with the stage on
        // failure. The instruction's error comes first when both fail.
        let diff = match (self.root.end_layer(), result) {
            (Ok(diff), Ok(())) => diff,
            (Err(e), Ok(())) => return Err(e.into()),
            (Ok(_), Err(e)) => return Err(e),
            (Err(end), Err(e)) => {
                tracing::warn!("closing the layer after a failed instruction: {end}");
                return Err(e);
            }
        };
        self.new_layers += 1;
        if self.input.demand.needed() {
            let blob = self.cx.exporter.export(Box::new(DiffLayer(diff)))?;
            self.config.push_layer(
                blob.diff_id.clone(),
                layer_history(created_by, self.cx.time),
            );
            self.layers.push(blob);
        } else {
            let _span = debug_span!("rnk_builder.layer", skipped = true).entered();
            drop(diff);
        }
        Ok(())
    }

    fn workdir(&mut self, dir: String, code: &str) -> Result<()> {
        self.edit(ConfigEdit::Workdir(dir))?;
        let wd = self.working_dir();
        let exists = match self.root.view()?.statx(&wd) {
            Ok(_) => true,
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NOENT.raw_os_error()) => false,
            Err(e) => return Err(e.into()),
        };
        if exists {
            return Ok(());
        }
        // kaniko creates it, owned by the active user, in a layer of its
        // own (`kaniko@44aa7ddc0 pkg/commands/workdir.go`).
        let user = self.user();
        self.layer(code, |_, root| {
            root.create_dir_all(&wd, 0o755)?;
            if !user.is_empty() {
                let u = rnk_rootfs::lookup_user(root, &user)?;
                let m = Meta {
                    uid: Some(u.uid),
                    gid: Some(u.gid),
                    ..Meta::default()
                };
                root.set_meta(&wd, &m)?;
            }
            Ok(())
        })
    }

    fn run_command(&mut self, run: rnk_dockerfile::Run, code: &str) -> Result<()> {
        if run.security == Security::Insecure {
            return Err(Error::Invalid(
                "RUN --security=insecure is not supported".to_owned(),
            ));
        }
        if !run.devices.is_empty() {
            return Err(Error::Invalid("RUN --device is not supported".to_owned()));
        }
        let shell = self
            .config
            .config
            .as_ref()
            .and_then(|c| c.shell.clone())
            .unwrap_or_else(|| rnk_oci::DEFAULT_SHELL.map(str::to_owned).to_vec());
        let workdir = self.working_dir();
        let mut mounts = self.mounts(&run.mounts, &workdir)?;
        let argv = match run_argv(&run.cmd, &shell)? {
            Argv::Plain(argv) => argv,
            Argv::Script { name, body } => {
                // BuildKit writes a `#!` heredoc to a file and runs it; the
                // file is bound in read-only from the build directory, so it
                // never reaches a layer.
                let script = write_script(self.cx.store.build_dir(), &body)?;
                let target = PathBuf::from("/dev/pipes").join(&name);
                mounts.push(BindMount {
                    source: script.path().to_owned(),
                    target: target.clone(),
                    kind: MountKind::File,
                    read_only: true,
                });
                self.scripts.push(script);
                vec![target.to_string_lossy().into_owned()]
            }
        };
        let network = match (run.network, self.cx.network) {
            (DfNetwork::None, _) => Network::None,
            // The operator's --network=none is a ceiling a Dockerfile cannot
            // lift [open: Q-184].
            (DfNetwork::Host, Network::None) => {
                return Err(Error::Invalid(
                    "RUN --network=host is refused: the build runs with --network=none".to_owned(),
                ));
            }
            (DfNetwork::Host, Network::Host) => Network::Host,
            (DfNetwork::Default, n) => n,
        };
        let env: Vec<(OsString, OsString)> = self
            .scope
            .env()
            .iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let user = self.user();
        self.layer(code, move |b, _root| {
            let mut spec = RunSpec::new(
                b.root.mount_point(),
                argv.into_iter().map(OsString::from).collect(),
            );
            spec.env = env;
            spec.user = user;
            spec.workdir = workdir;
            spec.network = network;
            spec.mounts = mounts;
            // Held until the RUN's process tree is gone (ADR-0013).
            let permit = b.cx.run_gate.enter().ok_or(Error::Cancelled)?;
            let status = b.cx.sandbox.run(spec);
            drop(permit);
            let status = status?;
            if status.success() {
                return Ok(());
            }
            use std::os::unix::process::ExitStatusExt as _;
            let code = status
                .code()
                .or_else(|| status.signal().map(|s| 128 + s))
                .unwrap_or(1);
            Err(Error::RunFailed { code })
        })
    }

    /// `RUN --mount` values as the sandbox's binds.
    fn mounts(&self, mounts: &[rnk_dockerfile::Mount], workdir: &Path) -> Result<Vec<BindMount>> {
        let mut out = Vec::new();
        for m in mounts {
            let target = |t: &str| mount_target(t, workdir);
            match m.kind {
                MountType::Cache => {
                    if !m.from.is_empty() {
                        return Err(Error::Invalid(
                            "RUN --mount=type=cache,from= is not supported yet (Phase 5)"
                                .to_owned(),
                        ));
                    }
                    if m.target.is_empty() {
                        return Err(Error::Invalid(
                            "RUN --mount=type=cache needs a target".to_owned(),
                        ));
                    }
                    // [open: Q-181] sharing= is not enforced: every cache is
                    // shared, as BuildKit's default.
                    let dir = self.cx.store.cache_dir(cache_id(m))?;
                    set_cache_owner(&dir, m)?;
                    out.push(BindMount {
                        source: dir,
                        target: target(&m.target),
                        kind: MountKind::Directory,
                        read_only: m.read_only,
                    });
                }
                MountType::Secret => {
                    if let Some(bind) = secret_mount(m, workdir, self.cx.secrets)? {
                        out.push(bind);
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
        Ok(out)
    }

    fn add(&mut self, index: usize, a: Add, code: &str) -> Result<()> {
        if !a.checksum.is_empty() || a.keep_git_dir.is_some() || a.unpack == Some(true) {
            return Err(Error::Invalid(
                "ADD --checksum, --keep-git-dir and --unpack are not supported yet (Phase 5)"
                    .to_owned(),
            ));
        }
        if let Some(s) = a.sources.iter().find(|s| is_remote(s)) {
            return Err(Error::Invalid(format!(
                "ADD of a URL or git repository ({s}) is not supported yet (Phase 5)"
            )));
        }
        let c = Copy {
            sources: a.sources,
            contents: a.contents,
            dest: a.dest,
            from: String::new(),
            chown: a.chown,
            chmod: a.chmod,
            link: a.link,
            parents: false,
            excludes: a.excludes,
        };
        self.copy_like(index, c, code, a.unpack != Some(false))
    }

    fn copy(&mut self, index: usize, c: Copy, code: &str) -> Result<()> {
        self.copy_like(index, c, code, false)
    }

    fn copy_like(
        &mut self,
        index: usize,
        c: Copy,
        code: &str,
        refuse_archives: bool,
    ) -> Result<()> {
        if c.link || c.parents || !c.excludes.is_empty() {
            return Err(Error::Invalid(
                "COPY --link, --parents and --exclude are not supported yet (Phase 5)".to_owned(),
            ));
        }
        let chmod = parse_chmod(&c.chmod)?;
        let dest = destination(&c.dest, &self.working_dir());
        let dest_is_dir = c.dest.ends_with('/') || c.dest == "." || c.dest.ends_with("/.");
        let cx = self.cx;
        let source = if c.from.is_empty() {
            CopySource::Context
        } else {
            copy_from(self.input, index)?
        };
        // A stage's or an image's root, held for the copy.
        let mut held = match &source {
            CopySource::Context => None,
            CopySource::Stage(root) => Some(root.lock()),
        };
        let src_root: &RootPath = match held.as_mut() {
            None => cx.context.root(),
            Some(guard) => guard
                .as_mut()
                .ok_or_else(|| Error::Invalid("the source stage was released".to_owned()))?
                .view()?,
        };
        for content in &c.contents {
            check_heredoc_name(&content.path)?;
        }
        let mut resolved = Vec::new();
        for s in &c.sources {
            let found = match &source {
                CopySource::Context => cx.context_files.resolve(s)?,
                CopySource::Stage(_) => sources::resolve_in_root(src_root, s)?,
            };
            resolved.extend(found);
        }
        if resolved.len() + c.contents.len() > 1 && !dest_is_dir {
            return Err(Error::Invalid(
                "when copying more than one source, the destination must be a directory \
                 and end with a /"
                    .to_owned(),
            ));
        }
        let from_context = matches!(source, CopySource::Context);
        let chown = c.chown.clone();
        let files = cx.context_files;
        let contents = c.contents;
        self.layer(code, |_, root| {
            let owner = if chown.is_empty() {
                // Docker's rule: context files belong to root; files from
                // a stage keep their owners.
                from_context.then_some(Owner { uid: 0, gid: 0 })
            } else {
                Some(rnk_rootfs::lookup_owner(root, &chown)?)
            };
            let opts = CopyOpts { owner, chmod };
            // An existing directory, or a symlink to one inside the root, as
            // BuildKit follows it: the copy goes inside.
            let into_dir = |root: &RootPath| -> Result<bool> {
                if dest_is_dir {
                    return Ok(true);
                }
                match root.open_at(&dest, &OpenHow::path().directory()) {
                    Ok(_) => Ok(true),
                    Err(e) if matches!(e.raw_os_error(), Some(c) if c == ENOENT || c == ENOTDIR) => {
                        Ok(false)
                    }
                    Err(e) => Err(e.into()),
                }
            };
            for r in &resolved {
                if r.dir {
                    if from_context {
                        // `r.path` has no symlink left: the entries below it
                        // are the walk's, filtered by `.dockerignore`.
                        let keep = |p: &Path, _dir: bool| files.keeps(p);
                        rnk_rootfs::copy_filtered(src_root, &r.path, root, &dest, &opts, &keep)?;
                    } else {
                        rnk_rootfs::copy(src_root, &r.path, root, &dest, &opts)?;
                    }
                    continue;
                }
                if refuse_archives && from_context && is_archive(src_root, &r.path)? {
                    return Err(Error::Invalid(format!(
                        "ADD of the local archive {} extracts it, which is not supported yet \
                         (Phase 5); use COPY and RUN tar",
                        r.named.display()
                    )));
                }
                let to = if into_dir(root)? {
                    dest.join(r.named.file_name().unwrap_or_default())
                } else {
                    dest.clone()
                };
                rnk_rootfs::copy(src_root, &r.path, root, &to, &opts)?;
            }
            for content in &contents {
                let to = if into_dir(root)? {
                    dest.join(&content.path)
                } else {
                    dest.clone()
                };
                write_content(root, &to, content, opts)?;
            }
            Ok(())
        })
    }
}

enum CopySource<'a, R> {
    Context,
    Stage(&'a SourceRoot<R>),
}

/// The root `COPY --from` at instruction `index` reads.
fn copy_from<'i, R>(input: &'i StageInput<'i, R>, index: usize) -> Result<CopySource<'i, R>> {
    let node = &input.graph.stages[input.index];
    let reference = node
        .references
        .iter()
        .find(|r| r.origin == Origin::Instruction(index) && r.kind == ReferenceKind::CopyFrom)
        .ok_or_else(|| Error::Invalid("COPY --from was not resolved".to_owned()))?;
    match &reference.source {
        Source::Stage(s) => input
            .stages
            .get(s)
            .map(|d| CopySource::Stage(&d.root))
            .ok_or_else(|| Error::Invalid(format!("stage {s} is not built"))),
        Source::Image(i) => input
            .images
            .get(i)
            .map(|d| CopySource::Stage(&d.root))
            .ok_or_else(|| Error::Invalid(format!("image {i} is not pulled"))),
    }
}

/// A closed layer as a [`LayerSource`]: the overlay upper, exported.
struct DiffLayer(LayerDiff);

impl LayerSource for DiffLayer {
    fn encode(
        self: Box<Self>,
        options: rnk_layer::EncodeOptions,
        out: &mut dyn std::io::Write,
    ) -> std::result::Result<rnk_layer::LayerDigests, rnk_layer::Error> {
        self.0
            .export(options, out)
            .map(|(digests, _)| digests)
            .map_err(|e| rnk_layer::Error::Io(std::io::Error::other(e)))
    }
}

/// `CMD` and `ENTRYPOINT` values as the config edit takes them.
fn command_line(cmd: CmdLine) -> CommandLine {
    if cmd.prepend_shell {
        CommandLine::Shell(cmd.args.into_iter().next().unwrap_or_default())
    } else {
        CommandLine::Exec(cmd.args)
    }
}

/// `HEALTHCHECK` as the config stores it, durations in nanoseconds.
fn health(h: &Healthcheck) -> HealthConfig {
    let nanos = |d: std::time::Duration| i64::try_from(d.as_nanos()).ok();
    HealthConfig {
        test: (!h.test.is_empty()).then(|| h.test.clone()),
        interval: nanos(h.interval),
        timeout: nanos(h.timeout),
        start_period: nanos(h.start_period),
        start_interval: nanos(h.start_interval),
        retries: Some(i64::from(h.retries)),
    }
}

/// What a `RUN` executes.
#[derive(Debug, PartialEq, Eq)]
enum Argv {
    /// This argv.
    Plain(Vec<String>),
    /// A `#!` heredoc: its body, written to a file named `name` and run.
    Script { name: String, body: String },
}

/// The argv of a `RUN`, as BuildKit builds it: exec form as written; shell
/// form with the stage's shell, heredocs included.
fn run_argv(cmd: &CmdLine, shell: &[String]) -> Result<Argv> {
    if !cmd.prepend_shell {
        if cmd.args.is_empty() {
            return Err(Error::Invalid("RUN has no command".to_owned()));
        }
        return Ok(Argv::Plain(cmd.args.clone()));
    }
    let line = cmd.args.first().cloned().unwrap_or_default();
    let mut argv = shell.to_vec();
    if cmd.files.is_empty() {
        argv.push(line);
        return Ok(Argv::Plain(argv));
    }
    // A RUN that is only one heredoc runs its body as the script.
    if let [file] = cmd.files.as_slice()
        && parse_heredoc_marker(line.trim()).is_some_and(|h| h.name == file.name)
    {
        let body = if file.chomp {
            chomp_heredoc_content(&file.data)
        } else {
            file.data.clone()
        };
        if body.starts_with("#!") {
            check_heredoc_name(&file.name)?;
            return Ok(Argv::Script {
                name: file.name.clone(),
                body,
            });
        }
        argv.push(body);
        return Ok(Argv::Plain(argv));
    }
    // Otherwise the shell reads the heredocs itself.
    let mut script = line;
    for f in &cmd.files {
        script.push('\n');
        script.push_str(&f.data);
        if !f.data.ends_with('\n') {
            script.push('\n');
        }
        script.push_str(&f.name);
    }
    argv.push(script);
    Ok(Argv::Plain(argv))
}

const ENOENT: i32 = rustix::io::Errno::NOENT.raw_os_error();
const ENOTDIR: i32 = rustix::io::Errno::NOTDIR.raw_os_error();

/// A heredoc's name names a file: one path component, not `.` or `..`.
fn check_heredoc_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(Error::Invalid(format!(
            "heredoc name {name:?} cannot name a file: one name without '/' is needed"
        )));
    }
    Ok(())
}

/// Writes a `#!` heredoc's body to a build-private executable file.
fn write_script(dir: &Path, body: &str) -> Result<tempfile::NamedTempFile> {
    let mut f = tempfile::Builder::new()
        .prefix(".heredoc-")
        .permissions(std::fs::Permissions::from_mode(0o755))
        .tempfile_in(dir)
        .map_err(Error::io("create a script in", dir))?;
    f.write_all(body.as_bytes())
        .map_err(Error::io("write", f.path().to_owned()))?;
    f.as_file()
        .sync_all()
        .map_err(Error::io("sync", f.path().to_owned()))?;
    Ok(f)
}

/// A `RUN --mount` target inside the root: absolute, or relative to the
/// working directory; cleaned, never above `/`.
fn mount_target(t: &str, workdir: &Path) -> PathBuf {
    let joined = if t.starts_with('/') {
        PathBuf::from(t)
    } else {
        workdir.join(t)
    };
    PathBuf::from("/").join(sources::clean_source(&joined.to_string_lossy()))
}

/// The id of a cache mount: `id=`, else its target, as BuildKit.
fn cache_id(m: &rnk_dockerfile::Mount) -> &str {
    if m.cache_id.is_empty() {
        &m.target
    } else {
        &m.cache_id
    }
}

/// Applies a cache mount's `mode`, `uid` and `gid` to its directory in the
/// store, through a descriptor of the directory itself: never by a path a
/// symlink could redirect. Only permission bits and the sticky bit are
/// taken from `mode`.
fn set_cache_owner(dir: &Path, m: &rnk_dockerfile::Mount) -> Result<()> {
    if m.mode.is_none() && m.uid.is_none() && m.gid.is_none() {
        return Ok(());
    }
    let root = RootPath::open(dir)?;
    let fd = root.open_at(".", &OpenHow::read().directory().no_follow())?;
    if let Some(mode) = m.mode {
        rustix::fs::fchmod(&fd, rustix::fs::Mode::from_raw_mode(mode & 0o1777))
            .map_err(|e| Error::io("chmod", dir)(e.into()))?;
    }
    if m.uid.is_some() || m.gid.is_some() {
        rustix::fs::fchown(
            &fd,
            m.uid.map(rustix::fs::Uid::from_raw),
            m.gid.map(rustix::fs::Gid::from_raw),
        )
        .map_err(|e| Error::io("chown", dir)(e.into()))?;
    }
    Ok(())
}

/// A `RUN --mount=type=secret` as a bind, `None` for a missing optional
/// secret. The id is `id=`, else the target's last name; the target is
/// `target=`, else `/run/secrets/<id>`.
fn secret_mount(
    m: &rnk_dockerfile::Mount,
    workdir: &Path,
    secrets: &BTreeMap<String, PathBuf>,
) -> Result<Option<BindMount>> {
    if m.env.is_some() {
        return Err(Error::Invalid(
            "RUN --mount=type=secret,env= is not supported yet (Q-022)".to_owned(),
        ));
    }
    let id = if m.cache_id.is_empty() {
        Path::new(&m.target)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        m.cache_id.clone()
    };
    if id.is_empty() {
        return Err(Error::Invalid(
            "RUN --mount=type=secret needs an id or a target".to_owned(),
        ));
    }
    let Some(source) = secrets.get(&id) else {
        if m.required {
            return Err(Error::Invalid(format!(
                "secret {id:?} is required but was not given (--secret)"
            )));
        }
        return Ok(None);
    };
    let target = if m.target.is_empty() {
        PathBuf::from("/run/secrets").join(&id)
    } else {
        mount_target(&m.target, workdir)
    };
    Ok(Some(BindMount {
        source: source.clone(),
        target,
        kind: MountKind::File,
        read_only: true,
    }))
}

/// `--chmod`, octal only [open: Q-120].
fn parse_chmod(s: &str) -> Result<Option<u32>> {
    if s.is_empty() {
        return Ok(None);
    }
    u32::from_str_radix(s, 8)
        .ok()
        .filter(|m| *m <= 0o7777)
        .map(Some)
        .ok_or_else(|| Error::Invalid(format!("--chmod={s}: only octal modes are supported")))
}

/// A `COPY` destination inside the root: absolute, or relative to the
/// working directory.
fn destination(dest: &str, workdir: &Path) -> PathBuf {
    let joined = if dest.starts_with('/') {
        PathBuf::from(dest)
    } else {
        workdir.join(dest)
    };
    PathBuf::from("/").join(sources::clean_source(&joined.to_string_lossy()))
}

fn is_remote(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.contains("://")
}

/// Whether a context file starts like an archive `ADD` would extract.
fn is_archive(root: &RootPath, path: &Path) -> Result<bool> {
    let mut f = std::fs::File::from(root.open_regular(path)?);
    let mut head = [0u8; 262];
    let mut n = 0;
    while n < head.len() {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err(Error::io("read", path)(e)),
        }
    }
    let h = &head[..n];
    Ok(h.starts_with(&[0x1f, 0x8b])
        || h.starts_with(b"BZh")
        || h.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0])
        || h.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
        || (n >= 262 && &h[257..262] == b"ustar"))
}

/// Writes a heredoc source of `COPY`/`ADD` to `to`.
fn write_content(
    root: &RootPath,
    to: &Path,
    content: &SourceContent,
    opts: CopyOpts,
) -> Result<()> {
    if let Some(parent) = to.parent() {
        root.create_dir_all(parent, 0o755)?;
    }
    match root.remove_file(to) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NOENT.raw_os_error()) => {}
        Err(e) => return Err(e.into()),
    }
    let fd = root.open_at(to, &OpenHow::write().create_new(0o644).no_follow())?;
    let mut f = std::fs::File::from(fd);
    f.write_all(content.data.as_bytes())
        .map_err(Error::io("write", to))?;
    let m = Meta {
        uid: opts.owner.map(|o| o.uid),
        gid: opts.owner.map(|o| o.gid),
        mode: Some(opts.chmod.unwrap_or(0o644)),
        ..Meta::default()
    };
    root.set_meta(to, &m)?;
    Ok(())
}

/// An instruction for messages: its first line, at most 80 characters.
fn shorten(code: &str) -> String {
    let line = code.lines().next().unwrap_or_default();
    if line.chars().count() > 80 {
        let cut: String = line.chars().take(77).collect();
        format!("{cut}...")
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use rnk_dockerfile::InlineFile;

    use super::*;

    fn shell() -> Vec<String> {
        vec!["/bin/sh".to_owned(), "-c".to_owned()]
    }

    #[test]
    fn run_argv_follows_buildkit() {
        let exec = CmdLine {
            args: vec!["echo".into(), "hi".into()],
            prepend_shell: false,
            files: vec![],
        };
        let plain = |v: &[&str]| Argv::Plain(v.iter().map(|s| (*s).to_owned()).collect());
        assert_eq!(run_argv(&exec, &shell()).unwrap(), plain(&["echo", "hi"]));
        let sh = CmdLine {
            args: vec!["echo $HOME".into()],
            prepend_shell: true,
            files: vec![],
        };
        assert_eq!(
            run_argv(&sh, &shell()).unwrap(),
            plain(&["/bin/sh", "-c", "echo $HOME"])
        );
        let only = CmdLine {
            args: vec!["<<EOF".into()],
            prepend_shell: true,
            files: vec![InlineFile {
                name: "EOF".into(),
                data: "\techo a\necho b\n".into(),
                chomp: false,
            }],
        };
        assert_eq!(
            run_argv(&only, &shell()).unwrap(),
            plain(&["/bin/sh", "-c", "\techo a\necho b\n"])
        );
        let chomped = CmdLine {
            files: vec![InlineFile {
                chomp: true,
                ..only.files[0].clone()
            }],
            args: vec!["<<-EOF".into()],
            ..only.clone()
        };
        assert_eq!(
            run_argv(&chomped, &shell()).unwrap(),
            plain(&["/bin/sh", "-c", "echo a\necho b\n"])
        );
        let inline = CmdLine {
            args: vec!["cat <<EOF > /f".into()],
            ..only.clone()
        };
        assert_eq!(
            run_argv(&inline, &shell()).unwrap(),
            plain(&["/bin/sh", "-c", "cat <<EOF > /f\n\techo a\necho b\nEOF"])
        );
        let shebang = CmdLine {
            args: vec!["<<EOF".into()],
            prepend_shell: true,
            files: vec![InlineFile {
                name: "EOF".into(),
                data: "#!/usr/bin/env python3\nprint(1)\n".into(),
                chomp: false,
            }],
        };
        assert_eq!(
            run_argv(&shebang, &shell()).unwrap(),
            Argv::Script {
                name: "EOF".into(),
                body: "#!/usr/bin/env python3\nprint(1)\n".into()
            }
        );
    }

    #[test]
    fn chmod_is_octal_only() {
        assert_eq!(parse_chmod("").unwrap(), None);
        assert_eq!(parse_chmod("755").unwrap(), Some(0o755));
        assert_eq!(parse_chmod("4755").unwrap(), Some(0o4755));
        assert!(parse_chmod("u+x").is_err());
        assert!(parse_chmod("17777").is_err());
    }

    #[test]
    fn destinations_resolve_against_the_working_directory() {
        let wd = Path::new("/app");
        assert_eq!(destination("x", wd), PathBuf::from("/app/x"));
        assert_eq!(destination("./", wd), PathBuf::from("/app"));
        assert_eq!(destination("/etc/", wd), PathBuf::from("/etc"));
        assert_eq!(destination("../../x", wd), PathBuf::from("/x"));
    }

    #[test]
    fn command_lines_and_health_map_to_config_edits() {
        let cmd = CmdLine {
            args: vec!["run.sh".into()],
            prepend_shell: true,
            files: vec![],
        };
        assert_eq!(command_line(cmd), CommandLine::Shell("run.sh".into()));
        let h = Healthcheck {
            test: vec!["CMD".into(), "true".into()],
            interval: std::time::Duration::from_secs(5),
            ..Healthcheck::default()
        };
        let c = health(&h);
        assert_eq!(c.interval, Some(5_000_000_000));
        assert_eq!(
            c.test.as_deref(),
            Some(&["CMD".to_owned(), "true".to_owned()][..])
        );
    }

    #[test]
    fn remote_sources_are_recognised() {
        assert!(is_remote("https://example.com/a.tgz"));
        assert!(is_remote("git@github.com:a/b.git"));
        assert!(!is_remote("src/a.tgz"));
    }

    fn mount(kind: MountType) -> rnk_dockerfile::Mount {
        rnk_dockerfile::Mount {
            kind,
            ..rnk_dockerfile::Mount::default()
        }
    }

    #[test]
    fn mount_targets_are_absolute_and_clean() {
        let wd = Path::new("/app");
        assert_eq!(mount_target("/cache", wd), PathBuf::from("/cache"));
        assert_eq!(
            mount_target("node_modules/.cache", wd),
            PathBuf::from("/app/node_modules/.cache")
        );
        assert_eq!(mount_target("../../../x", wd), PathBuf::from("/x"));
    }

    #[test]
    fn a_cache_id_defaults_to_the_target() {
        let mut m = mount(MountType::Cache);
        m.target = "/root/.npm".into();
        assert_eq!(cache_id(&m), "/root/.npm");
        m.cache_id = "npm".into();
        assert_eq!(cache_id(&m), "npm");
    }

    #[test]
    fn secret_mounts_follow_buildkit_defaults() {
        let wd = Path::new("/app");
        let secrets: BTreeMap<String, PathBuf> =
            [("token".to_owned(), PathBuf::from("/host/token"))].into();
        // The id from the target's last name.
        let mut m = mount(MountType::Secret);
        m.target = "/etc/token".into();
        let b = secret_mount(&m, wd, &secrets).unwrap().unwrap();
        assert_eq!(b.source, PathBuf::from("/host/token"));
        assert_eq!(b.target, PathBuf::from("/etc/token"));
        assert!(b.read_only);
        // The target from the id; a relative target under the workdir.
        let mut m = mount(MountType::Secret);
        m.cache_id = "token".into();
        assert_eq!(
            secret_mount(&m, wd, &secrets).unwrap().unwrap().target,
            PathBuf::from("/run/secrets/token")
        );
        m.target = "conf/token".into();
        assert_eq!(
            secret_mount(&m, wd, &secrets).unwrap().unwrap().target,
            PathBuf::from("/app/conf/token")
        );
        // Missing: skipped unless required.
        let mut m = mount(MountType::Secret);
        m.cache_id = "other".into();
        assert_eq!(secret_mount(&m, wd, &secrets).unwrap(), None);
        m.required = true;
        assert!(secret_mount(&m, wd, &secrets).is_err());
        // No id at all, and env=.
        assert!(secret_mount(&mount(MountType::Secret), wd, &secrets).is_err());
        let mut m = mount(MountType::Secret);
        m.cache_id = "token".into();
        m.env = Some("TOKEN".into());
        assert!(
            secret_mount(&m, wd, &secrets)
                .unwrap_err()
                .to_string()
                .contains("Q-022")
        );
    }

    #[test]
    fn heredoc_names_name_one_file() {
        assert!(check_heredoc_name("EOF").is_ok());
        for bad in ["", ".", "..", "a/b", "../x"] {
            assert!(check_heredoc_name(bad).is_err(), "{bad:?}");
        }
    }
}
