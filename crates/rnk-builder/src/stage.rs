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
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rnk_context::Context;
use rnk_dockerfile::{
    Add, CmdLine, Copy, Healthcheck, Instruction, Located, Network as DfNetwork, Origin,
    ReferenceKind, Security, Source, SourceContent, Stage, StageGraph, StageScope,
    chomp_heredoc_content, parse_heredoc_marker,
};
use rnk_oci::{CommandLine, ConfigEdit, HealthConfig, ImageConfig, ImageFormat, Platform};
use rnk_rootfs::{Chmod, CopyOpts, Meta, OpenHow, Owner, RootPath};
use rnk_sandbox::{BindMount, Isolation, MountKind, Network, RunSpec, Sandbox};
use rnk_snapshot::{Base, ExportLayer, Keep, Snapshotter, StageRootfs};
use rnk_store::Store;
use tracing::{debug_span, info, info_span};

use crate::error::{Error, Result};
use crate::history;
use crate::image::instruction_history;
use crate::layer::{BlobLayer, LayerDemand, LayerExporter, LayerSource};
use crate::mounts::{self, Secret};
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
    /// How `RUN` is isolated: the full sandbox in overlay mode,
    /// `Isolation::Direct` on kaniko mode's shared root (ADR-0043).
    pub isolation: Isolation,
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
    /// `RUN --mount=type=secret` sources by id: `--secret` (ADR-0032).
    pub secrets: &'a BTreeMap<String, Secret>,
    /// Whether a `RUN --network=host` may lift `--network=none`: BuildKit's
    /// `network.host` entitlement, `--allow network.host` (ADR-0032).
    pub allow_network_host: bool,
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

    /// Keeps what later stages read of this finished stage before its root
    /// is reused (kaniko mode's shared root; overlay mode keeps every root
    /// anyway): the paths that `need` names, or all of it.
    ///
    /// For `COPY --from` sources, the entries each source names are kept
    /// with the symlinks on their way, and the kept copy must name the same
    /// entries again: when it does not (a source that expands at build
    /// time, a link out of the copy), the whole root is kept instead.
    ///
    /// # Errors
    ///
    /// As `StageRootfs::keep`, and [`Error::Invalid`] when the root was
    /// released.
    pub fn keep(&self, need: &KeepNeed) -> Result<()> {
        let mut guard = self.lock();
        let root = guard
            .as_mut()
            .ok_or_else(|| Error::Invalid("the stage to keep was released".to_owned()))?;
        let KeepNeed::Sources(list) = need else {
            return Ok(root.keep(&Keep::All)?);
        };
        let Some((paths, expected)) = keep_paths(root.view()?, list) else {
            return Ok(root.keep(&Keep::All)?);
        };
        // A source naming the root (`/`, `.`, a link to `/`) is the whole
        // root; a path the snapshotter refuses (another mount, a link on
        // the way) is left to the whole-root copy, which leaves out what
        // the ignore list holds.
        if paths.iter().any(|p| p.as_os_str().is_empty()) {
            return Ok(root.keep(&Keep::All)?);
        }
        if let Err(e) = root.keep(&Keep::Paths(paths)) {
            tracing::debug!("keeping the sources by path: {e}; keeping the whole stage");
            return Ok(root.keep(&Keep::All)?);
        }
        let kept = root.view()?;
        let same = list
            .iter()
            .map(|s| sources::resolve_in_root(kept, s).ok())
            .eq(expected.into_iter().map(Some));
        if !same {
            tracing::debug!("the kept sources resolve differently: keeping the whole stage");
            root.keep(&Keep::All)?;
        }
        Ok(())
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

/// What later stages read of a stage, kept before its root is reused
/// ([`SourceRoot::keep`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeepNeed {
    /// All of it: a stage built `FROM` it, `RUN --mount from=` it, or a
    /// source not known before the build (a variable in it).
    All,
    /// The sources of the `COPY --from` instructions that read it, as
    /// written.
    Sources(Vec<String>),
}

/// The paths to keep for `list` in `root`: each source's entries with the
/// symlinks on their way, and what each source resolves to now. `None`
/// when a source cannot be resolved here.
#[allow(clippy::type_complexity, reason = "a pair returned once")]
fn keep_paths(
    root: &RootPath,
    list: &[String],
) -> Option<(Vec<PathBuf>, Vec<Vec<sources::Resolved>>)> {
    let mut paths = Vec::new();
    let mut expected = Vec::with_capacity(list.len());
    for s in list {
        let found = sources::resolve_in_root(root, s).ok()?;
        for entry in &found {
            for p in sources::link_closure(root, &entry.path).ok()? {
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
        expected.push(found);
    }
    Some((paths, expected))
}

/// An image read by `COPY --from=<image>`: pulled, with a root on it, made
/// by the stage that reads it (preparation touches no root).
#[derive(Debug)]
pub struct ImageSource<R, L> {
    /// The pulled image.
    pub image: std::sync::Arc<PulledImage<L>>,
    /// A root on its layers.
    pub root: SourceRoot<R>,
}

impl<R: StageRootfs, L> ImageSource<R, L> {
    /// A root on `image`'s layers.
    ///
    /// # Errors
    ///
    /// As `Snapshotter::prepare`.
    pub fn new<S: Snapshotter<Root = R, Layer = L>>(
        snapshotter: &S,
        image: std::sync::Arc<PulledImage<L>>,
    ) -> Result<Self> {
        let root = snapshotter.prepare_source(&image.layers)?;
        Ok(Self {
            image,
            root: SourceRoot::new(root),
        })
    }
}

/// What a stage starts from.
pub enum StageBase<'a, R, L> {
    /// `FROM scratch`.
    Scratch,
    /// `FROM <image>`.
    Image(&'a PulledImage<L>),
    /// `FROM <stage>`.
    Stage(&'a StageDone<R>),
}

/// One stage to build.
pub struct StageInput<'a, R, L> {
    /// The stage's index in the graph.
    pub index: usize,
    /// The stage graph.
    pub graph: &'a StageGraph,
    /// The stage's instructions, as parsed.
    pub stage: &'a Stage,
    /// Its base.
    pub base: StageBase<'a, R, L>,
    /// Finished stages it reads with `COPY --from`, by index.
    pub stages: BTreeMap<usize, &'a StageDone<R>>,
    /// Images it reads with `COPY --from`, by reference as written.
    pub images: BTreeMap<String, &'a ImageSource<R, L>>,
    /// Which of its layers are encoded.
    pub demand: LayerDemand,
    /// kaniko's `--label`s, set on the stage's config before its first
    /// instruction, so a `LABEL` of the same key wins. kaniko gives them to
    /// the pushed stage only (`kaniko@44aa7ddc0 pkg/executor/build.go:190-191,
    /// 275-285`): empty for every other stage. ADR-0036.
    pub labels: &'a [(String, String)],
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
    input: StageInput<'_, S::Root, S::Layer>,
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
            let config = ImageConfig {
                architecture: cx.platform.architecture.clone(),
                os: cx.platform.os.clone(),
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
    let mut root = root;
    root.set_layers_needed(input.demand.needed());
    let mut config = config;
    with_default_path(&mut config);
    let mut b = Builder {
        cx,
        input: &input,
        name: node.display_name.clone(),
        root,
        scope,
        config,
        layers,
        new_layers: 0,
        cmd_set: false,
        scripts: Vec::new(),
    };
    let result = if input.labels.is_empty() {
        Ok(())
    } else {
        ConfigEdit::Label(input.labels.to_vec())
            .apply(&mut b.config)
            .map_err(|e| Error::Invalid(format!("--label: {e}")))
    }
    .and_then(|()| b.run());
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
    input: &'i StageInput<'i, S::Root, S::Layer>,
    name: String,
    root: S::Root,
    scope: StageScope,
    config: ImageConfig,
    layers: Vec<BlobLayer>,
    new_layers: u64,
    /// Whether this stage ran a `CMD`, which a later `ENTRYPOINT` keeps.
    cmd_set: bool,
    /// Scripts of `#!` heredocs, removed when the stage ends.
    scripts: Vec<tempfile::TempDir>,
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
        // Each instruction adds its history entry as BuildKit's dispatcher
        // does (Q-131, `ADR-0032`); `ONBUILD` adds none.
        match expanded.value {
            Instruction::Arg(decls) => {
                self.history(&history::arg(&decls));
                Ok(())
            }
            Instruction::Env(kvs) => {
                let text: String = kvs
                    .iter()
                    .map(|kv| format!(" {}={}", kv.key, kv.value))
                    .collect();
                let pairs = kvs.into_iter().map(|kv| (kv.key, kv.value)).collect();
                self.edit(ConfigEdit::Env(pairs), &format!("ENV{text}"))
            }
            Instruction::Label(kvs) => {
                let text: String = kvs
                    .iter()
                    .map(|kv| format!(" {}={}", kv.key, kv.value))
                    .collect();
                let pairs = kvs.into_iter().map(|kv| (kv.key, kv.value)).collect();
                self.edit(ConfigEdit::Label(pairs), &format!("LABEL{text}"))
            }
            Instruction::User(user) => {
                let text = format!("USER {user}");
                self.edit(ConfigEdit::User(user), &text)
            }
            Instruction::Expose(ports) => {
                let text = format!("EXPOSE {}", rnk_oci::exposed_ports(&ports)?.join(" "));
                self.edit(ConfigEdit::Expose(ports), &text)
            }
            // Config only, as BuildKit: the directories are not created
            // (Q-180, `ADR-0032`).
            Instruction::Volume(volumes) => {
                let text = format!("VOLUME {}", history::go_list(&volumes));
                self.edit(ConfigEdit::Volume(volumes), &text)
            }
            Instruction::Entrypoint(cmd) => {
                let edit = ConfigEdit::Entrypoint {
                    command: command_line(cmd),
                    keep_cmd: self.cmd_set,
                };
                edit.apply(&mut self.config)?;
                let args = self
                    .run_field(|c| c.entrypoint.as_ref())
                    .unwrap_or_default();
                self.history(&format!("ENTRYPOINT {}", history::go_quoted_list(&args)));
                Ok(())
            }
            Instruction::Cmd(cmd) => {
                ConfigEdit::Cmd(command_line(cmd)).apply(&mut self.config)?;
                self.cmd_set = true;
                let args = self.run_field(|c| c.cmd.as_ref()).unwrap_or_default();
                self.history(&format!("CMD {}", history::go_quoted_list(&args)));
                Ok(())
            }
            Instruction::Shell(shell) => {
                let text = format!("SHELL {}", history::go_list(&shell));
                self.edit(ConfigEdit::Shell(shell), &text)
            }
            Instruction::StopSignal(signal) => {
                let text = format!("STOPSIGNAL {signal}");
                self.edit(ConfigEdit::StopSignal(signal), &text)
            }
            Instruction::Healthcheck(h) => self.edit(
                ConfigEdit::Healthcheck(health(&h)),
                &history::healthcheck(&h),
            ),
            Instruction::Onbuild(trigger) => {
                ConfigEdit::OnBuild(trigger).apply(&mut self.config)?;
                Ok(())
            }
            Instruction::Maintainer(name) => {
                self.history(&format!("MAINTAINER {name}"));
                self.config.author = Some(name);
                Ok(())
            }
            Instruction::Workdir(dir) => self.workdir(dir),
            Instruction::Run(run) => self.run_command(run),
            Instruction::Copy(c) => self.copy(index, c),
            Instruction::Add(a) => self.add(index, a),
        }
    }

    /// Applies a metadata edit and adds its history entry.
    fn edit(&mut self, edit: ConfigEdit, text: &str) -> Result<()> {
        edit.apply(&mut self.config)?;
        self.history(text);
        Ok(())
    }

    /// Adds the history entry of an instruction that made no layer.
    fn history(&mut self, text: &str) {
        self.config
            .push_history(instruction_history(text, self.cx.time));
    }

    /// A field of the config's `config` object.
    fn run_field<T: Clone>(
        &self,
        f: impl FnOnce(&rnk_oci::ContainerConfig) -> Option<&T>,
    ) -> Option<T> {
        self.config.config.as_ref().and_then(f).cloned()
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
            self.cx.exporter.exported(&blob, self.input.demand)?;
            self.config.push_layer(
                blob.diff_id.clone(),
                instruction_history(created_by, self.cx.time),
            );
            self.layers.push(blob);
        } else {
            let _span = debug_span!("rnk_builder.layer", skipped = true).entered();
            drop(diff);
        }
        Ok(())
    }

    /// `WORKDIR`, as BuildKit's `dispatchWorkdir` (`buildkit@cd8ee244
    /// frontend/dockerfile/dockerfile2llb/convert.go:1513-1570`): a directory
    /// other than `/` is made with `mkdir -p` in a layer of its own, even
    /// when it exists (an empty layer then), the directories it creates
    /// owned by the active user, mode `0755`.
    fn workdir(&mut self, dir: String) -> Result<()> {
        ConfigEdit::Workdir(dir).apply(&mut self.config)?;
        let wd = self.working_dir();
        let written = self
            .run_field(|c| c.working_dir.as_ref())
            .unwrap_or_else(|| "/".to_owned());
        let text = format!("WORKDIR {written}");
        // BuildKit compares the string: `/./`, kept as written, is not `/`
        // and gets its (empty) layer (`convert.go:1546`).
        if written == "/" {
            self.history(&text);
            return Ok(());
        }
        let user = self.user();
        self.layer(&text, |_, root| make_workdir(root, &wd, &user))
    }

    fn run_command(&mut self, run: rnk_dockerfile::Run) -> Result<()> {
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
        let network = run_network(run.network, self.cx.network, self.cx.allow_network_host)?;
        let mut mounts = mounts::run_mounts(
            &run.mounts,
            &workdir,
            self.cx.store,
            self.cx.secrets,
            self.cx.cancelled,
        )?;
        let argv = match run_argv(&run.cmd, &shell)? {
            Argv::Plain(argv) => argv,
            Argv::Script { name, body } => {
                // BuildKit writes a `#!` heredoc to a file and runs it from
                // a directory bound read-only at /dev/pipes; the directory
                // lives in the build directory, so it never reaches a layer.
                let script = write_script(self.cx.store.build_dir(), &name, &body)?;
                mounts.binds.push(BindMount {
                    source: script.path().to_owned(),
                    target: PathBuf::from(DEV_PIPES),
                    kind: MountKind::Directory,
                    read_only: true,
                });
                self.scripts.push(script);
                vec![format!("{DEV_PIPES}/{name}")]
            }
        };
        // Secrets given with `env=` come last and win over a variable of the
        // same name; they live in this RUN's environment only.
        let mut env: Vec<(OsString, OsString)> = self
            .scope
            .env()
            .iter()
            .map(|(k, v)| (k.into(), v.into()))
            .filter(|(k, _): &(OsString, OsString)| mounts.env.iter().all(|(s, _)| s != k))
            .collect();
        env.append(&mut mounts.env);
        let user = self.user();
        let text = format!(
            "{} # buildkit",
            history::run(&argv, self.scope.args(), self.scope.env())
        );
        self.layer(&text, move |b, _root| {
            let mut spec = RunSpec::new(
                b.root.mount_point(),
                argv.into_iter().map(OsString::from).collect(),
            );
            spec.isolation = b.cx.isolation;
            spec.env = env;
            spec.user = user;
            spec.workdir = workdir;
            spec.network = network;
            spec.mounts = std::mem::take(&mut mounts.binds);
            spec.secrets = std::mem::take(&mut mounts.secrets);
            // Held until the RUN's process tree is gone (ADR-0013).
            let Some(permit) = b.cx.run_gate.enter() else {
                mounts.finish();
                return Err(Error::Cancelled);
            };
            // In kaniko mode on `/`, no named state exists on disk while
            // the RUN's processes do, nor is any made (Q-340, ADR-0056).
            let fence = match b.cx.store.enter_run() {
                Ok(fence) => fence,
                Err(e) => {
                    drop(permit);
                    mounts.finish();
                    return Err(e.into());
                }
            };
            let status = b.cx.sandbox.run(spec);
            // An error once the command ran may leave the RUN's processes
            // alive (a failed sweep): no name is made for the rest of the
            // build. The state directory must still be the one the store
            // holds.
            let swept = status
                .as_ref()
                .map_or_else(rnk_sandbox::Error::ran_no_command, |_| true);
            let left = b.cx.store.leave_run(fence, swept);
            drop(permit);
            // The RUN is gone: its cache leases end.
            mounts.finish();
            left?;
            let status = match status {
                Err(rnk_sandbox::Error::Cancelled) => return Err(Error::Cancelled),
                s => s?,
            };
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

    fn add(&mut self, index: usize, a: Add) -> Result<()> {
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
        self.copy_like(index, c, true, a.unpack != Some(false))
    }

    fn copy(&mut self, index: usize, c: Copy) -> Result<()> {
        self.copy_like(index, c, false, false)
    }

    fn copy_like(
        &mut self,
        index: usize,
        c: Copy,
        is_add: bool,
        refuse_archives: bool,
    ) -> Result<()> {
        if c.link || c.parents || !c.excludes.is_empty() {
            return Err(Error::Invalid(
                "COPY --link, --parents and --exclude are not supported yet (Phase 5)".to_owned(),
            ));
        }
        // A source that expanded to nothing would name the whole context or
        // stage root (ADR-0052); `.` names it on purpose.
        if c.sources.iter().any(String::is_empty) {
            return Err(Error::Invalid(format!(
                "a {} source expands to an empty string; write `.` to copy the whole {}",
                if is_add { "ADD" } else { "COPY" },
                if c.from.is_empty() {
                    "context"
                } else {
                    "stage"
                }
            )));
        }
        let chmod = parse_chmod(&c.chmod)?;
        let heredocs: Vec<String> = c.contents.iter().map(|h| h.path.clone()).collect();
        let text = format!(
            "{} # buildkit",
            history::copy(is_add, &c.chown, &c.chmod, &c.sources, &heredocs, &c.dest)
        );
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
        // BuildKit has no "several sources need a destination ending in /"
        // rule: each source is copied in turn, a later file over an earlier
        // one when the destination is not a directory (Q-080, `ADR-0032`).
        let from_context = matches!(source, CopySource::Context);
        let chown = c.chown.clone();
        let files = cx.context_files;
        let contents = c.contents;
        self.layer(&text, |_, root| {
            let owner = if chown.is_empty() {
                // Docker's rule: context files belong to root; files from
                // a stage keep their owners.
                from_context.then_some(Owner { uid: 0, gid: 0 })
            } else {
                Some(rnk_rootfs::lookup_owner(root, &chown)?)
            };
            let opts = CopyOpts {
                owner,
                chmod: chmod.clone(),
            };
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
                write_content(root, &to, content, &opts)?;
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
fn copy_from<'i, R, L>(input: &'i StageInput<'i, R, L>, index: usize) -> Result<CopySource<'i, R>> {
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

/// A closed layer as a [`LayerSource`]: an overlay upper or a kaniko-mode
/// walk diff, exported.
struct DiffLayer<D>(D);

impl<D: ExportLayer> LayerSource for DiffLayer<D> {
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

/// Where a `#!` heredoc's script directory is bound, as BuildKit's
/// `dockerfile2llb`; `rnk-sandbox` makes it in the `/dev` tmpfs.
const DEV_PIPES: &str = "/dev/pipes";

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
fn write_script(dir: &Path, name: &str, body: &str) -> Result<tempfile::TempDir> {
    // 0755: a RUN as another USER enters the bound directory too.
    let d = tempfile::Builder::new()
        .prefix(".heredoc-")
        .permissions(std::fs::Permissions::from_mode(0o755))
        .tempdir_in(dir)
        .map_err(Error::io("create a script directory in", dir))?;
    let path = d.path().join(name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&path)
        .map_err(Error::io("create", path.clone()))?;
    f.write_all(body.as_bytes())
        .map_err(Error::io("write", path.clone()))?;
    f.sync_all().map_err(Error::io("sync", path))?;
    Ok(d)
}

/// The network of a `RUN`. As BuildKit's `network.host` entitlement, a
/// `RUN --network=host` under the operator's `--network=none` needs
/// `--allow network.host`; under the default host network (ADR-0010) it
/// asks for nothing more.
fn run_network(asked: DfNetwork, build: Network, allow_host: bool) -> Result<Network> {
    match (asked, build) {
        (DfNetwork::None, _) => Ok(Network::None),
        (DfNetwork::Host, Network::None) if !allow_host => Err(Error::Invalid(
            "RUN --network=host: network.host is not allowed under --network=none; \
             pass --allow network.host"
                .to_owned(),
        )),
        (DfNetwork::Host, _) => Ok(Network::Host),
        (DfNetwork::Default, n) => Ok(n),
    }
}

/// `--chmod`: octal or symbolic, as BuildKit (Q-120).
fn parse_chmod(s: &str) -> Result<Option<Chmod>> {
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(Chmod::parse(s)?))
}

/// Adds BuildKit's default `PATH` to a config without one, except on
/// Windows (`buildkit@cd8ee244 frontend/dockerfile/dockerfile2llb/convert.go:792-802`;
/// Q-185, `ADR-0032`). `FROM scratch` gets it this way too.
fn with_default_path(config: &mut ImageConfig) {
    if config.os == "windows" {
        return;
    }
    let run = config.config.get_or_insert_with(Default::default);
    let env = run.env.get_or_insert_with(Vec::new);
    if !env.iter().any(|e| e.split('=').next() == Some("PATH")) {
        env.push(format!("PATH={}", rnk_dockerfile::DEFAULT_PATH));
    }
}

/// `mkdir -p` of the working directory `wd` inside `root`, symlinks
/// followed inside the root, as BuildKit's `MkdirAll`: each directory it
/// creates gets mode `0755` and, when `user` is set, that user's ids.
fn make_workdir(root: &RootPath, wd: &Path, user: &str) -> Result<()> {
    let ids = if user.is_empty() {
        None
    } else {
        Some(rnk_rootfs::lookup_user(root, user)?)
    };
    let mut at = PathBuf::from("/");
    for c in wd.components() {
        at.push(c);
        match root.open_at(&at, &OpenHow::path().directory()) {
            Ok(_) => continue,
            Err(e) if e.raw_os_error() == Some(ENOENT) => {}
            Err(e) => return Err(e.into()),
        }
        root.create_dir_all(&at, 0o755)?;
        if let Some(u) = &ids {
            let m = Meta {
                uid: Some(u.uid),
                gid: Some(u.gid),
                ..Meta::default()
            };
            root.set_meta(&at, &m)?;
        }
    }
    Ok(())
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
    opts: &CopyOpts,
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
        mode: Some(opts.chmod.as_ref().map_or(0o644, |c| c.apply(0o644, false))),
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

    /// A kaniko-mode snapshotter on a temp root, its keep area beside it.
    fn kaniko() -> (tempfile::TempDir, PathBuf, rnk_snapshot::Kaniko) {
        let tmp = tempfile::tempdir().unwrap();
        let host = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(host.join("root")).unwrap();
        std::fs::create_dir(host.join("kept")).unwrap();
        let k = rnk_snapshot::Kaniko::new(
            RootPath::open(host.join("root")).unwrap(),
            host.join("root"),
            rnk_rootfs::IgnoreList::new(),
            rnk_snapshot::WalkOptions::default(),
            RootPath::open(host.join("kept")).unwrap(),
        );
        (tmp, host.join("root"), k)
    }

    fn names(r: Vec<sources::Resolved>) -> Vec<(PathBuf, bool)> {
        r.into_iter().map(|r| (r.path, r.dir)).collect()
    }

    #[test]
    fn copy_from_sources_are_kept_with_the_links_on_their_way() {
        use std::os::unix::fs::symlink;
        let (_tmp, host, k) = kaniko();
        let root = SourceRoot::new(k.prepare(Base::Scratch).unwrap());
        std::fs::create_dir_all(host.join("opt/app/dist")).unwrap();
        std::fs::write(host.join("opt/app/dist/a.js"), b"a").unwrap();
        std::fs::write(host.join("opt/app/dist/b.css"), b"b").unwrap();
        std::fs::write(host.join("opt/app/secret"), b"s").unwrap();
        symlink("/opt/app", host.join("app")).unwrap();
        let sources = vec!["/app/dist/*.js".to_owned()];
        root.keep(&KeepNeed::Sources(sources.clone())).unwrap();
        // The root is reused: only the kept copy answers now.
        let other = k.prepare(Base::Scratch).unwrap();
        let mut guard = root.lock();
        let view = guard.as_mut().unwrap().view().unwrap();
        assert_eq!(
            names(sources::resolve_in_root(view, &sources[0]).unwrap()),
            [(PathBuf::from("app/dist/a.js"), false)]
        );
        assert!(view.statx("opt/app/secret").is_err(), "only what is read");
        assert!(view.statx("opt/app/dist/b.css").is_err());
        drop(guard);
        other.release().unwrap();
        root.release().unwrap();
    }

    #[test]
    fn a_source_that_resolves_differently_keeps_the_whole_stage() {
        let (_tmp, host, k) = kaniko();
        let root = SourceRoot::new(k.prepare(Base::Scratch).unwrap());
        std::fs::write(host.join("a"), b"a").unwrap();
        std::fs::write(host.join("b"), b"b").unwrap();
        // A source missing now cannot be kept by its paths.
        root.keep(&KeepNeed::Sources(vec!["/missing".to_owned()]))
            .unwrap();
        let mut guard = root.lock();
        let view = guard.as_mut().unwrap().view().unwrap();
        assert!(view.statx("a").is_ok() && view.statx("b").is_ok());
        drop(guard);
        root.release().unwrap();
    }

    #[test]
    fn a_link_closure_follows_links_inside_the_root() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path();
        std::fs::create_dir_all(host.join("real/dir")).unwrap();
        symlink("real", host.join("rel")).unwrap();
        symlink("/rel/dir", host.join("abs")).unwrap();
        symlink("../../real", host.join("real/dir/up")).unwrap();
        let root = RootPath::open(host).unwrap();
        assert_eq!(
            sources::link_closure(&root, Path::new("abs/up")).unwrap(),
            [
                PathBuf::from("abs"),
                PathBuf::from("rel"),
                PathBuf::from("real/dir/up"),
                PathBuf::from("real"),
            ]
        );
        symlink("loop", host.join("loop")).unwrap();
        assert!(sources::link_closure(&root, Path::new("loop")).is_err());
    }

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
    fn chmod_is_octal_or_symbolic() {
        assert_eq!(parse_chmod("").unwrap(), None);
        assert_eq!(parse_chmod("755").unwrap(), Some(Chmod::Octal(0o755)));
        assert_eq!(parse_chmod("4755").unwrap(), Some(Chmod::Octal(0o4755)));
        assert!(matches!(
            parse_chmod("u+x").unwrap(),
            Some(Chmod::Symbolic(_))
        ));
        assert!(parse_chmod("17777").is_err());
        assert!(parse_chmod("u+q").is_err());
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

    #[test]
    fn host_network_under_none_needs_the_entitlement() {
        use DfNetwork as Df;
        assert_eq!(
            run_network(Df::Default, Network::None, false).unwrap(),
            Network::None
        );
        assert_eq!(
            run_network(Df::Host, Network::Host, false).unwrap(),
            Network::Host
        );
        let refused = run_network(Df::Host, Network::None, false).unwrap_err();
        assert!(refused.to_string().contains("--allow network.host"));
        assert_eq!(
            run_network(Df::Host, Network::None, true).unwrap(),
            Network::Host
        );
        assert_eq!(
            run_network(Df::None, Network::Host, false).unwrap(),
            Network::None
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
