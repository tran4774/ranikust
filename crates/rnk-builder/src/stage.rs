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
use std::os::unix::fs::{PermissionsExt as _, chown};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rnk_context::Context;
use rnk_dockerfile::{
    Add, CmdLine, Copy, Healthcheck, Instruction, Located, MountType, Network as DfNetwork, Origin,
    ReferenceKind, Security, Source, SourceContent, Stage, StageGraph, StageScope,
    chomp_heredoc_content, parse_heredoc_marker,
};
use rnk_oci::{CommandLine, ConfigEdit, HealthConfig, ImageConfig, ImageFormat, Platform};
use rnk_rootfs::{CopyOpts, FileType, Meta, OpenHow, Owner, RootPath};
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
    /// `RUN --mount=type=secret` sources: id to host file.
    // [open: Q-022] interim: `--secret id=ID,src=PATH`, as BuildKit's CLI.
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

/// An image read by `COPY --from=<image>`: pulled, with a root on it.
#[derive(Debug)]
pub struct ImageSource<R> {
    /// The pulled image.
    pub image: PulledImage,
    /// A root on its layers.
    pub root: SourceRoot<R>,
}

impl<R: StageRootfs> ImageSource<R> {
    /// A root on `image`'s layers.
    ///
    /// # Errors
    ///
    /// As `Snapshotter::prepare`.
    pub fn new<S: Snapshotter<Root = R>>(snapshotter: &S, image: PulledImage) -> Result<Self> {
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
    let mut b = Builder {
        cx,
        input: &input,
        name: node.display_name.clone(),
        root,
        scope,
        config,
        layers,
        new_layers: 0,
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
            self.execute(index, instruction).map_err(|e| match e {
                Error::Cancelled => Error::Cancelled,
                e => Error::Instruction {
                    stage: self.name.clone(),
                    line: instruction.lines.start,
                    instruction: shorten(&instruction.code),
                    source: Box::new(e),
                },
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
        // failure.
        let diff = self.root.end_layer()?;
        result?;
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

    fn run_command(&mut self, mut run: rnk_dockerfile::Run, code: &str) -> Result<()> {
        if run.security == Security::Insecure {
            return Err(Error::Invalid(
                "RUN --security=insecure is not supported".to_owned(),
            ));
        }
        if !run.devices.is_empty() {
            return Err(Error::Invalid("RUN --device is not supported".to_owned()));
        }
        if run.mounts.len() != run.mount_specs.len() {
            run.parse_mounts(&mut |s| Ok(s.to_owned()))
                .map_err(Error::Invalid)?;
        }
        let shell = self
            .config
            .config
            .as_ref()
            .and_then(|c| c.shell.clone())
            .unwrap_or_else(|| rnk_oci::DEFAULT_SHELL.map(str::to_owned).to_vec());
        let argv = run_argv(&run.cmd, &shell)?;
        let workdir = self.working_dir();
        let mounts = self.mounts(&run.mounts, &workdir)?;
        let network = match run.network {
            DfNetwork::None => Network::None,
            DfNetwork::Host => Network::Host,
            DfNetwork::Default => self.cx.network,
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
            let target = |t: &str| -> PathBuf {
                if t.starts_with('/') {
                    PathBuf::from(t)
                } else {
                    workdir.join(t)
                }
            };
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
                    let id = if m.cache_id.is_empty() {
                        &m.target
                    } else {
                        &m.cache_id
                    };
                    let dir = self.cx.store.cache_dir(id)?;
                    if let Some(mode) = m.mode {
                        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))
                            .map_err(Error::io("chmod", &dir))?;
                    }
                    if m.uid.is_some() || m.gid.is_some() {
                        chown(&dir, m.uid, m.gid).map_err(Error::io("chown", &dir))?;
                    }
                    out.push(BindMount {
                        source: dir,
                        target: target(&m.target),
                        kind: MountKind::Directory,
                        read_only: m.read_only,
                    });
                }
                MountType::Secret => {
                    if m.env.is_some() {
                        return Err(Error::Invalid(
                            "RUN --mount=type=secret,env= is not supported yet".to_owned(),
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
                    let Some(source) = self.cx.secrets.get(&id) else {
                        if m.required {
                            return Err(Error::Invalid(format!(
                                "secret {id:?} is required but was not given (--secret)"
                            )));
                        }
                        continue;
                    };
                    let target = if m.target.is_empty() {
                        PathBuf::from("/run/secrets").join(&id)
                    } else {
                        target(&m.target)
                    };
                    out.push(BindMount {
                        source: source.clone(),
                        target,
                        kind: MountKind::File,
                        read_only: true,
                    });
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
            let into_dir = |root: &RootPath| -> Result<bool> {
                Ok(dest_is_dir
                    || matches!(root.statx(&dest), Ok(st) if st.file_type() == FileType::Directory))
            };
            for (path, is_dir) in &resolved {
                if *is_dir {
                    if from_context {
                        let keep = |p: &Path, _dir: bool| files.keeps(p);
                        rnk_rootfs::copy_filtered(src_root, path, root, &dest, &opts, &keep)?;
                    } else {
                        rnk_rootfs::copy(src_root, path, root, &dest, &opts)?;
                    }
                    continue;
                }
                if refuse_archives && from_context && is_archive(src_root, path)? {
                    return Err(Error::Invalid(format!(
                        "ADD of the local archive {} extracts it, which is not supported yet \
                         (Phase 5); use COPY and RUN tar",
                        path.display()
                    )));
                }
                let to = if into_dir(root)? {
                    dest.join(path.file_name().unwrap_or_default())
                } else {
                    dest.clone()
                };
                rnk_rootfs::copy(src_root, path, root, &to, &opts)?;
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

/// The argv of a `RUN`, as BuildKit builds it: exec form as written; shell
/// form with the stage's shell, heredocs included.
fn run_argv(cmd: &CmdLine, shell: &[String]) -> Result<Vec<String>> {
    if !cmd.prepend_shell {
        if cmd.args.is_empty() {
            return Err(Error::Invalid("RUN has no command".to_owned()));
        }
        return Ok(cmd.args.clone());
    }
    let line = cmd.args.first().cloned().unwrap_or_default();
    let mut argv = shell.to_vec();
    if cmd.files.is_empty() {
        argv.push(line);
        return Ok(argv);
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
            return Err(Error::Invalid(
                "a RUN heredoc with a #! line is not supported yet".to_owned(),
            ));
        }
        argv.push(body);
        return Ok(argv);
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
    Ok(argv)
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
        assert_eq!(run_argv(&exec, &shell()).unwrap(), ["echo", "hi"]);
        let sh = CmdLine {
            args: vec!["echo $HOME".into()],
            prepend_shell: true,
            files: vec![],
        };
        assert_eq!(
            run_argv(&sh, &shell()).unwrap(),
            ["/bin/sh", "-c", "echo $HOME"]
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
            ["/bin/sh", "-c", "\techo a\necho b\n"]
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
            ["/bin/sh", "-c", "echo a\necho b\n"]
        );
        let inline = CmdLine {
            args: vec!["cat <<EOF > /f".into()],
            ..only.clone()
        };
        assert_eq!(
            run_argv(&inline, &shell()).unwrap(),
            ["/bin/sh", "-c", "cat <<EOF > /f\n\techo a\necho b\nEOF"]
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
}
