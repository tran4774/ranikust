//! The build after the set-up: the Dockerfile and context, the stages run
//! by `rnk-sched` through `rnk-builder`, then the outputs in kaniko's order
//! (OCI layout, Docker tar, push).
//!
//! The scheduler polls every stage on one task: each stage is built on a
//! thread of its own, which asks the task for a `--jobs` permit before each
//! `RUN` (ADR-0013). Stage roots are released here, children before
//! parents, on success and on failure alike; the store is closed last.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use rnk_builder::{
    BuildCx, BuildTime, ContextFiles, Error, ImageSource, LayerDemand, LayerExporter, Outputs,
    PulledImage, RunGate, StageBase, StageDone, StageInput, assemble_annotated, build_stage,
    image_format, pull_image,
};
use rnk_context::{Context, ContextSpec};
use rnk_dockerfile::{Dockerfile, GraphOptions, StageGraph};
use rnk_layer::{Compression, EncodeOptions};
use rnk_oci::Platform;
use rnk_registry::{Client, ClientOptions, Credentials, RetryPolicy, TrustStore};
use rnk_sandbox::Sandbox;
use rnk_sched::{CancellationToken, PrepareJob, Scheduler, StageJob, StageRunner};
use rnk_snapshot::{Overlay, OverlayStage};
use rnk_store::Store;

use crate::config::{self, BuildConfig};

type Stage = StageDone<OverlayStage>;
type ImageRoot = ImageSource<OverlayStage>;

/// Everything the stages share; owned by the build, borrowed by each
/// stage's thread through an `Arc`.
struct Shared {
    overlay: Arc<Overlay>,
    sandbox: Sandbox,
    store: Arc<Store>,
    context: Context,
    context_files: ContextFiles,
    exporter: LayerExporter,
    time: BuildTime,
    network: rnk_sandbox::Network,
    secrets: BTreeMap<String, rnk_builder::Secret>,
    allow_network_host: bool,
    platform: Platform,
    client: Arc<Client>,
    dockerfile: Dockerfile,
    graph: StageGraph,
    outputs: Outputs,
    /// Per stage: whether the target's image inherits its layers.
    in_target: Vec<bool>,
    /// `--label`s, for the pushed stage.
    labels: Vec<(String, String)>,
    /// Every root made, for the release at the end.
    stages: Mutex<Vec<(usize, Arc<Stage>)>>,
    images: Mutex<Vec<Arc<ImageRoot>>>,
}

/// What a build reports on success.
pub(crate) struct Built {
    /// The image digest.
    pub(crate) digest: String,
}

/// Runs the build on what the set-up made. Consumes the sandbox and the
/// store, which it closes.
pub(crate) fn run(
    config: &BuildConfig,
    store: Arc<Store>,
    sandbox: Sandbox,
    overlay_caps: rnk_snapshot::OverlayCaps,
) -> Result<Built, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting the async runtime: {e}"))?;
    let prepared = prepare(config, Arc::clone(&store), sandbox, overlay_caps);
    let shared = match prepared {
        Ok(s) => Arc::new(s),
        Err(e) => {
            close_store(store);
            return Err(e);
        }
    };
    let result = runtime.block_on(build(config, &shared));
    release_all(&shared);
    drop(runtime);
    match Arc::into_inner(shared) {
        Some(s) => {
            drop(s.overlay);
            drop(s.sandbox);
            drop(s.exporter);
        }
        None => tracing::warn!("a stage thread still holds the build; the store stays open"),
    }
    close_store(store);
    result
}

fn close_store(store: Arc<Store>) {
    match Arc::into_inner(store) {
        Some(store) => {
            if let Err(e) = store.close() {
                tracing::warn!("closing the store: {e}");
            }
        }
        None => tracing::warn!("the store is still in use; the next build cleans it up"),
    }
}

/// Reads the Dockerfile and the context and makes the shared state.
fn prepare(
    config: &BuildConfig,
    store: Arc<Store>,
    sandbox: Sandbox,
    overlay_caps: rnk_snapshot::OverlayCaps,
) -> Result<Shared, String> {
    let context = rnk_context::open(&ContextSpec::Dir(config.context.clone()))
        .map_err(|e| format!("opening the build context: {e}"))?;
    let source = dockerfile_source(&config.dockerfile);
    let text = read_dockerfile(&source, &context)?;
    let dockerfile_path = match &source {
        DockerfileSource::Host(p) => p.clone(),
        DockerfileSource::Context(rel) => context.origin().join(rel),
    };
    let dockerfile = rnk_dockerfile::parse(&text).map_err(|e| e.to_string())?;
    let (os, arch) = config
        .platform
        .split_once('/')
        .unwrap_or(("linux", "amd64"));
    let df_platform = rnk_dockerfile::Platform::new(os, arch);
    let options = GraphOptions {
        build_args: config
            .build_args
            .iter()
            .filter_map(|a| match &a.value {
                Some(v) => Some(Ok((a.name.clone(), v.clone()))),
                // kaniko: `--build-arg NAME` takes the value from the
                // environment, and an unset variable leaves it unset.
                None => env_var(&a.name)
                    .transpose()
                    .map(|v| v.map(|v| (a.name.clone(), v))),
            })
            .collect::<Result<_, String>>()?,
        build_platform: df_platform.clone(),
        target_platform: df_platform,
        targets: config.targets.clone(),
    };
    let graph = rnk_dockerfile::stage_graph(&dockerfile, &options).map_err(|e| e.to_string())?;

    let ignore = context
        .find_dockerignore(Some(&dockerfile_path))
        .map_err(|e| e.to_string())?;
    let matcher = ignore
        .as_ref()
        .map(|i| context.matcher(i))
        .transpose()
        .map_err(|e| e.to_string())?;
    let context_files =
        ContextFiles::load(&context, matcher.as_ref()).map_err(|e| e.to_string())?;

    let outputs = Outputs::new(
        &config.destinations,
        !config.no_push,
        config.tar_path.clone(),
        config.oci_layout_path.clone(),
    )
    .map_err(|e| e.to_string())?;
    let time = if config.reproducible {
        BuildTime::reproducible(env_var("SOURCE_DATE_EPOCH")?.as_deref())
            .map_err(|e| e.to_string())?
    } else {
        BuildTime::at(SystemTime::now())
    };
    let threads = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let compression = match config.compression {
        config::Compression::Gzip => Compression::Gzip,
        config::Compression::Zstd => Compression::Zstd,
    };
    let encode = EncodeOptions {
        compression,
        level: config.compression_level.unwrap_or(match compression {
            Compression::Zstd => 3,
            Compression::Gzip | Compression::None => 6,
        }),
        threads,
        mtime: None,
    };
    let exporter = LayerExporter::new(store.build_dir(), encode, &time);

    let options = client_options(config, &graph)?;
    let credentials = credentials()?;
    let client = Client::new(options, TrustStore::load(), credentials)
        .map_err(|e| format!("setting up the registry client: {e}"))?;
    let overlay =
        Arc::new(Overlay::new(Arc::clone(&store), overlay_caps).map_err(|e| e.to_string())?);

    let in_target = in_target(&graph);
    let mut secrets = BTreeMap::new();
    for (id, source) in &config.secrets {
        // As BuildKit's store: a file secret must exist and fit at the
        // start, not first at the RUN that mounts it.
        source
            .check(id)
            .map_err(|e| format!("--secret id={id}: {e}"))?;
        secrets.insert(id.clone(), source.clone());
    }
    Ok(Shared {
        overlay,
        sandbox,
        store,
        context,
        context_files,
        exporter,
        time,
        network: match config.network {
            config::Network::Host => rnk_sandbox::Network::Host,
            config::Network::None => rnk_sandbox::Network::None,
        },
        secrets,
        allow_network_host: config.allow_network_host,
        platform: Platform::new(os, arch),
        client: Arc::new(client),
        dockerfile,
        graph,
        outputs,
        in_target,
        labels: config.labels.clone(),
        stages: Mutex::new(Vec::new()),
        images: Mutex::new(Vec::new()),
    })
}

/// Where the Dockerfile is read from.
#[derive(Debug, PartialEq, Eq)]
enum DockerfileSource {
    /// A path the operator gave that exists as given: read on the host.
    Host(PathBuf),
    /// Inside the context, relative to its root: read through it, so a
    /// symlink there never leaves the context.
    Context(PathBuf),
}

/// kaniko's `--dockerfile`: as given when it exists, else inside the
/// context (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go` `resolveDockerfilePath`).
fn dockerfile_source(dockerfile: &Path) -> DockerfileSource {
    if dockerfile.is_absolute() || dockerfile.exists() {
        return DockerfileSource::Host(
            std::path::absolute(dockerfile).unwrap_or_else(|_| dockerfile.to_owned()),
        );
    }
    DockerfileSource::Context(rnk_dockerfile_rel(dockerfile))
}

/// A relative Dockerfile path inside the context, `..` clamped.
fn rnk_dockerfile_rel(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::Normal(n) => out.push(n),
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::RootDir
            | std::path::Component::CurDir
            | std::path::Component::Prefix(_) => {}
        }
    }
    out
}

/// The Dockerfile's text.
fn read_dockerfile(source: &DockerfileSource, context: &Context) -> Result<String, String> {
    use std::io::Read as _;
    let mut text = String::new();
    match source {
        DockerfileSource::Host(p) => std::fs::File::open(p)
            .and_then(|mut f| f.read_to_string(&mut text))
            .map_err(|e| format!("reading the Dockerfile {}: {e}", p.display()))?,
        DockerfileSource::Context(rel) => {
            let fd = context.root().open_regular(rel).map_err(|e| {
                format!(
                    "reading the Dockerfile {} in the context: {e}",
                    rel.display()
                )
            })?;
            std::fs::File::from(fd)
                .read_to_string(&mut text)
                .map_err(|e| format!("reading the Dockerfile {}: {e}", rel.display()))?
        }
    };
    Ok(text)
}

/// An environment variable: `None` when unset, an error when not Unicode.
fn env_var(name: &str) -> Result<Option<String>, String> {
    match std::env::var_os(name) {
        None => Ok(None),
        Some(v) => v
            .into_string()
            .map(Some)
            .map_err(|_| format!("the environment variable {name} is not valid UTF-8")),
    }
}

/// The registry client's settings: kaniko's per-direction flags turned into
/// per-registry sets (ADR-0036). Destinations are pushed to; base images and
/// `COPY --from` images are pulled from. A registry in both gets the union.
fn client_options(config: &BuildConfig, graph: &StageGraph) -> Result<ClientOptions, String> {
    let registry = |r: &str| {
        rnk_oci::Reference::parse(r)
            .map(|r| r.registry().to_owned())
            .map_err(|e| format!("{r}: {e}"))
    };
    let pushed: Vec<String> = config
        .destinations
        .iter()
        .map(|d| registry(d))
        .collect::<Result<_, _>>()?;
    let mut pulled = Vec::new();
    for node in &graph.stages {
        if let rnk_dockerfile::Base::Image(r) = &node.base {
            pulled.push(registry(r)?);
        }
        for r in node.images() {
            pulled.push(registry(r)?);
        }
    }
    let mut options = ClientOptions::default();
    options.registry_certificates = config.registry_certificates.clone();
    options.insecure = config.insecure_registries.iter().cloned().collect();
    if config.insecure_push {
        options.insecure.extend(pushed.iter().cloned());
    }
    if config.insecure_pull {
        options.insecure.extend(pulled.iter().cloned());
    }
    options.skip_tls_verify = config.skip_tls_verify_registries.iter().cloned().collect();
    if config.skip_tls_verify_push {
        options.skip_tls_verify.extend(pushed);
    }
    if config.skip_tls_verify_pull {
        options.skip_tls_verify.extend(pulled);
    }
    options.push_retry = config.push_retry.map(RetryPolicy::kaniko);
    options.pull_retry = config.image_download_retry.map(RetryPolicy::kaniko);
    Ok(options)
}

/// The credentials kaniko reads, in its order: `DOCKER_AUTH_CONFIG` first,
/// then the docker CLI's `config.json` (`kaniko@44aa7ddc0
/// pkg/creds/creds.go:39-49`); none when both are missing.
fn credentials() -> Result<Credentials, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = Credentials::config_path(
        std::env::var_os("DOCKER_CONFIG").as_deref().map(OsStr::new),
        home.as_deref(),
    );
    let file = match path {
        Some(p) if p.exists() => Credentials::load(&p).map_err(|e| e.to_string())?,
        _ => Credentials::none(),
    };
    match std::env::var_os("DOCKER_AUTH_CONFIG") {
        Some(v) if !v.is_empty() => Ok(Credentials::from_env_value(v.as_encoded_bytes())
            .map_err(|e| e.to_string())?
            .or(file)),
        _ => Ok(file),
    }
}

/// kaniko's digest files (`kaniko@44aa7ddc0 pkg/executor/push.go:194-256`):
/// the digest alone, then `repository[:tag]@digest` lines, one per
/// destination, with go-containerregistry's names (Docker Hub as
/// `index.docker.io`, a missing tag as `latest`).
fn write_digest_files(config: &BuildConfig, digest: &str) -> Result<(), String> {
    let write = |path: &Path, text: &str| {
        std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))
    };
    if let Some(path) = &config.digest_file {
        write(path, digest)?;
    }
    if config.image_name_digest_file.is_none() && config.image_name_tag_digest_file.is_none() {
        return Ok(());
    }
    let mut plain = String::new();
    let mut tagged = String::new();
    for d in &config.destinations {
        let r = rnk_oci::Reference::parse(d).map_err(|e| format!("{d}: {e}"))?;
        let registry = match r.registry() {
            "docker.io" => "index.docker.io",
            other => other,
        };
        let repository = format!("{registry}/{}", r.repository());
        plain.push_str(&format!("{repository}@{digest}\n"));
        let tag = r.tag().unwrap_or("latest");
        tagged.push_str(&format!("{repository}:{tag}@{digest}\n"));
    }
    // kaniko builds one text, with tags when the tag file is asked for, and
    // writes it to both files (`push.go:231-256`).
    let text = if config.image_name_tag_digest_file.is_some() {
        &tagged
    } else {
        &plain
    };
    if let Some(path) = &config.image_name_digest_file {
        write(path, text)?;
    }
    if let Some(path) = &config.image_name_tag_digest_file {
        write(path, text)?;
    }
    Ok(())
}

/// Per stage: whether the target's image inherits its layers, through
/// `FROM <stage>` from the primary target.
fn in_target(graph: &StageGraph) -> Vec<bool> {
    let mut out = vec![false; graph.stages.len()];
    let mut at = Some(graph.primary_target());
    while let Some(s) = at {
        out[s] = true;
        at = match graph.stages[s].base {
            rnk_dockerfile::Base::Stage(b) => Some(b),
            rnk_dockerfile::Base::Scratch | rnk_dockerfile::Base::Image(_) => None,
        };
    }
    out
}

/// The stages, then the outputs.
async fn build(config: &BuildConfig, shared: &Arc<Shared>) -> Result<Built, String> {
    let runner = Runner {
        shared: Arc::clone(shared),
    };
    let cancel = CancellationToken::new();
    let signals = tokio::spawn(cancel_on_signals(cancel.clone()));
    let summary = Scheduler::new(config.jobs)
        .run(&shared.graph, &runner, &cancel)
        .await;
    signals.abort();
    let summary = summary.map_err(|e| e.to_string())?;
    let target = summary
        .stages
        .iter()
        .find(|(i, _)| *i == summary.target)
        .map(|(_, d)| Arc::clone(d))
        .ok_or("the target stage was not built")?;
    let done: &Stage = &target;
    let format = match config.image_format {
        Some(config::ImageFormatArg::Docker) => rnk_oci::ImageFormat::Docker,
        Some(config::ImageFormatArg::Oci) => rnk_oci::ImageFormat::Oci,
        None => image_format(),
    };
    let image = assemble_annotated(
        done.config.clone(),
        done.layers.clone(),
        format,
        &shared.time,
        &config.annotations,
    )
    .map_err(|e| e.to_string())?;
    write_digest_files(config, image.digest().as_str())?;
    let outputs = &shared.outputs;
    if let Some(dir) = &outputs.oci_layout {
        rnk_builder::write_oci_layout(&image, dir).map_err(|e| e.to_string())?;
    }
    if let Some(path) = &outputs.docker_tar {
        rnk_builder::write_docker_tar(&image, outputs.tar_tags(), path)
            .map_err(|e| e.to_string())?;
    }
    if outputs.push {
        let jobs = config
            .jobs
            .or_else(|| std::thread::available_parallelism().ok())
            .unwrap_or(NonZeroUsize::MIN);
        rnk_builder::push(&image, &outputs.destinations, &shared.client, jobs)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(Built {
        digest: image.digest().to_string(),
    })
}

/// Cancels the build on the first SIGINT or SIGTERM: running `RUN`s are
/// killed, as BuildKit does (ADR-0032), and stages stop. A second
/// signal exits at once: the `RUN`s die with their parent thread
/// (`PR_SET_PDEATHSIG`), the mounts with the private mount namespace, and
/// the next build cleans the store.
async fn cancel_on_signals(cancel: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut int), Ok(mut term)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) else {
        tracing::warn!("cannot handle SIGINT and SIGTERM; they end the build at once");
        return;
    };
    for n in 0..2 {
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
        }
        if n == 0 {
            tracing::warn!("cancelling the build; a second signal stops at once");
            cancel.cancel();
        } else {
            tracing::warn!("stopping at once");
            std::process::exit(130);
        }
    }
}

/// Releases every root, children before parents: later stages first, the
/// roots of `COPY --from` images last.
fn release_all(shared: &Shared) {
    let mut stages =
        std::mem::take(&mut *shared.stages.lock().unwrap_or_else(PoisonError::into_inner));
    stages.sort_by_key(|(i, _)| std::cmp::Reverse(*i));
    // A stage whose child could not be released still has that child's
    // overlay on its diffs: it is left to the next build's clean-up.
    let mut keep = std::collections::BTreeSet::new();
    for (i, stage) in stages {
        if keep.contains(&i) {
            tracing::warn!(
                "leaving stage {i} to the next build: a stage built on it is still mounted"
            );
            continue;
        }
        if let Err(e) = stage.root.release() {
            tracing::warn!("releasing stage {i}: {e}");
            let mut at = shared.graph.stages[i].base.clone();
            while let rnk_dockerfile::Base::Stage(b) = at {
                keep.insert(b);
                at = shared.graph.stages[b].base.clone();
            }
        }
    }
    let images = std::mem::take(&mut *shared.images.lock().unwrap_or_else(PoisonError::into_inner));
    for image in images {
        if let Err(e) = image.root.release() {
            tracing::warn!("releasing the root of {}: {e}", image.image.reference);
        }
    }
}

struct Runner {
    shared: Arc<Shared>,
}

/// What a stage's preparation pulled; no root is made yet.
struct Prepared {
    base: Option<Arc<PulledImage>>,
    images: BTreeMap<String, Arc<PulledImage>>,
}

/// Pulls a stage's base image and the images its `COPY --from` reads.
async fn prepare_stage(
    shared: &Arc<Shared>,
    base: Option<String>,
    images: Vec<String>,
) -> Result<Prepared, Error> {
    let mut prepared = Prepared {
        base: None,
        images: BTreeMap::new(),
    };
    if let Some(r) = base {
        let image = pull_image(
            &shared.client,
            &shared.store,
            &shared.overlay,
            &r,
            &shared.platform,
        )
        .await?;
        prepared.base = Some(Arc::new(image));
    }
    for r in images {
        let image = pull_image(
            &shared.client,
            &shared.store,
            &shared.overlay,
            &r,
            &shared.platform,
        )
        .await?;
        prepared.images.insert(r, Arc::new(image));
    }
    Ok(prepared)
}

/// Waits for a stage's pulls to end, even once `cancel` fires: the pull
/// runs on a blocking thread that keeps using the runtime and the store,
/// so returning early would leave it running while both shut down. A
/// cancel is reported once it ends.
async fn await_pulls<T>(
    work: tokio::task::JoinHandle<Result<T, Error>>,
    cancel: &CancellationToken,
) -> Result<T, Error> {
    let r = work
        .await
        .unwrap_or_else(|e| Err(Error::Invalid(format!("pulling: {e}"))));
    if cancel.is_cancelled() && r.is_ok() {
        return Err(Error::Cancelled);
    }
    r
}

impl StageRunner for Runner {
    type Prepared = Arc<Prepared>;
    type Output = Arc<Stage>;
    type Error = Error;

    fn prepare(
        &self,
        job: PrepareJob<'_>,
    ) -> impl Future<Output = Result<Arc<Prepared>, Error>> + Send {
        let shared = Arc::clone(&self.shared);
        let node = job.node;
        let cancel = job.cancel.clone();
        let base = match &node.base {
            rnk_dockerfile::Base::Image(r) => Some(r.clone()),
            rnk_dockerfile::Base::Scratch | rnk_dockerfile::Base::Stage(_) => None,
        };
        let platform_ok = node
            .platform
            .as_deref()
            .is_none_or(|p| p == config::PLATFORM);
        let images: Vec<String> = node.images().into_iter().map(str::to_owned).collect();
        async move {
            if !platform_ok {
                return Err(Error::Invalid(format!(
                    "FROM --platform other than {} is not supported (ADR-0009)",
                    config::PLATFORM
                )));
            }
            // The registry client's futures are not `Send` for every
            // lifetime, which this future must be: the pulls run on a
            // blocking thread that drives them. They are awaited even
            // after a cancel: an abandoned pull would still use the
            // runtime while it shuts down, and the store's fill must end
            // before the store closes.
            let handle = tokio::runtime::Handle::current();
            let work = tokio::task::spawn_blocking(move || {
                handle
                    .block_on(prepare_stage(&shared, base, images))
                    .map(Arc::new)
            });
            await_pulls(work, &cancel).await
        }
    }

    fn build(
        &self,
        job: StageJob<'_, Self>,
    ) -> impl Future<Output = Result<Arc<Stage>, Error>> + Send {
        let shared = Arc::clone(&self.shared);
        async move {
            let index = job.stage;
            let deps: Vec<(usize, Arc<Stage>)> = job
                .deps
                .iter()
                .map(|(i, d)| (*i, Arc::clone(&**d)))
                .collect();
            let (requests, mut asked) = tokio::sync::mpsc::unbounded_channel();
            let (done_tx, mut done) = tokio::sync::oneshot::channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&cancelled);
            let s = Arc::clone(&shared);
            let prepared = Arc::clone(&job.prepared);
            std::thread::Builder::new()
                .name(format!("stage-{index}"))
                .spawn(move || {
                    let gate = ChannelGate { requests };
                    let r = run_stage(&s, index, &deps, &prepared, &gate, &flag);
                    // Let go of the build before reporting, so that the
                    // caller can close the store once every stage is done.
                    drop((s, deps, prepared, gate));
                    // The receiver is gone only when the build future was
                    // dropped, which the scheduler never does.
                    let _ = done_tx.send(r);
                })
                .map_err(|e| {
                    Error::Invalid(format!("starting the thread of stage {index}: {e}"))
                })?;
            let mut seen_cancel = false;
            loop {
                tokio::select! {
                    r = &mut done => {
                        return r.unwrap_or_else(|_| Err(Error::Invalid(
                            format!("the thread of stage {index} ended without a result"),
                        )));
                    }
                    Some(reply) = asked.recv() => {
                        let reply: std::sync::mpsc::SyncSender<Option<tokio::sync::oneshot::Sender<()>>> = reply;
                        match job.run_slot().await {
                            None => {
                                // A stage thread that stopped waiting has
                                // nothing to be told.
                                let _ = reply.send(None);
                            }
                            Some(permit) => {
                                let (release, released) = tokio::sync::oneshot::channel();
                                if reply.send(Some(release)).is_ok() {
                                    // Held until the RUN's process tree exits;
                                    // an error means the thread dropped its
                                    // permit without sending, which also
                                    // releases it. A cancel meanwhile must
                                    // reach the RUN, or it would run to its
                                    // end first.
                                    let mut released = released;
                                    tokio::select! {
                                        _ = &mut released => {}
                                        () = job.cancel.cancelled(), if !seen_cancel => {
                                            seen_cancel = true;
                                            cancelled.store(true, Ordering::Relaxed);
                                            shared.sandbox.cancel();
                                            let _ = released.await;
                                        }
                                    }
                                }
                                drop(permit);
                            }
                        }
                    }
                    () = job.cancel.cancelled(), if !seen_cancel => {
                        seen_cancel = true;
                        cancelled.store(true, Ordering::Relaxed);
                        // As BuildKit, a cancel kills the running RUNs; the
                        // stages then stop with Error::Cancelled.
                        shared.sandbox.cancel();
                    }
                }
            }
        }
    }
}

/// The scheduler's `--jobs` permits, asked for from a stage's thread.
struct ChannelGate {
    requests: tokio::sync::mpsc::UnboundedSender<
        std::sync::mpsc::SyncSender<Option<tokio::sync::oneshot::Sender<()>>>,
    >,
}

/// A held permit: dropping it hands the permit back.
struct Held(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            // The stage's task waits for this; if it is gone, so is the
            // permit.
            let _ = release.send(());
        }
    }
}

impl RunGate for ChannelGate {
    fn enter(&self) -> Option<Box<dyn Send>> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        self.requests.send(reply).ok()?;
        let release = answer.recv().ok()??;
        Some(Box::new(Held(Some(release))))
    }
}

/// Builds stage `index` on the calling thread.
fn run_stage(
    shared: &Shared,
    index: usize,
    deps: &[(usize, Arc<Stage>)],
    prepared: &Prepared,
    gate: &ChannelGate,
    cancelled: &AtomicBool,
) -> Result<Arc<Stage>, Error> {
    let node = &shared.graph.stages[index];
    let base = match &node.base {
        rnk_dockerfile::Base::Scratch => StageBase::Scratch,
        rnk_dockerfile::Base::Image(_) => StageBase::Image(
            prepared
                .base
                .as_deref()
                .ok_or_else(|| Error::Invalid("the base image was not pulled".to_owned()))?,
        ),
        rnk_dockerfile::Base::Stage(b) => StageBase::Stage(
            deps.iter()
                .find(|(i, _)| i == b)
                .map(|(_, d)| &**d)
                .ok_or_else(|| Error::Invalid(format!("stage {b} is not built")))?,
        ),
    };
    // The roots of `COPY --from` images are made here, by the stage that
    // reads them, and released with the build.
    let mut sources = Vec::with_capacity(prepared.images.len());
    for (r, image) in &prepared.images {
        let root = Arc::new(ImageSource::new(&*shared.overlay, Arc::clone(image))?);
        shared
            .images
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&root));
        sources.push((r.clone(), root));
    }
    let input = StageInput {
        index,
        graph: &shared.graph,
        stage: &shared.dockerfile.stages[index],
        base,
        stages: deps.iter().map(|(i, d)| (*i, &**d)).collect(),
        images: sources.iter().map(|(k, v)| (k.clone(), &**v)).collect(),
        demand: LayerDemand::for_stage(shared.in_target[index], &shared.outputs, false),
        labels: if index == shared.graph.primary_target() {
            &shared.labels
        } else {
            &[]
        },
    };
    let is_cancelled = || cancelled.load(Ordering::Relaxed);
    let cx = BuildCx {
        snapshotter: &*shared.overlay,
        sandbox: &shared.sandbox,
        store: &shared.store,
        context: &shared.context,
        context_files: &shared.context_files,
        exporter: &shared.exporter,
        time: &shared.time,
        network: shared.network,
        secrets: &shared.secrets,
        allow_network_host: shared.allow_network_host,
        platform: &shared.platform,
        run_gate: gate,
        cancelled: &is_cancelled,
    };
    let done = Arc::new(build_stage(&cx, input)?);
    shared
        .stages
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((index, Arc::clone(&done)));
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_inherits_layers_of_its_from_stages_only() {
        let df = rnk_dockerfile::parse(
            "FROM alpine AS a\nFROM a AS b\nFROM alpine AS c\nCOPY --from=b / /\nFROM c\n",
        )
        .unwrap();
        let graph = rnk_dockerfile::stage_graph(&df, &GraphOptions::default()).unwrap();
        assert_eq!(in_target(&graph), [false, false, true, true]);
    }

    #[test]
    fn a_dockerfile_not_found_as_given_is_read_inside_the_context() {
        assert_eq!(
            dockerfile_source(Path::new("no-such-dir/../x/Dockerfile")),
            DockerfileSource::Context(PathBuf::from("x/Dockerfile"))
        );
        assert_eq!(
            dockerfile_source(Path::new("/abs/Dockerfile")),
            DockerfileSource::Host(PathBuf::from("/abs/Dockerfile"))
        );
        let ctx = tempfile::tempdir().unwrap();
        std::fs::write(ctx.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "SECRET first line\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), ctx.path().join("escape")).unwrap();
        let context = rnk_context::open(&ContextSpec::Dir(ctx.path().to_owned())).unwrap();
        let ok = DockerfileSource::Context(PathBuf::from("Dockerfile"));
        assert_eq!(read_dockerfile(&ok, &context).unwrap(), "FROM scratch\n");
        // A symlink in the context resolves inside it: the host file is
        // never read.
        let link = DockerfileSource::Context(PathBuf::from("escape"));
        let e = read_dockerfile(&link, &context).unwrap_err();
        assert!(!e.contains("SECRET"), "{e}");
    }

    fn config_of(flags: &[&str]) -> BuildConfig {
        let argv = std::iter::once("ranikust").chain(flags.iter().copied());
        let crate::cli::Command::Build(args) = crate::cli::parse(argv).unwrap().cli.into_command()
        else {
            panic!("expected build");
        };
        BuildConfig::from_args(&args).unwrap()
    }

    /// kaniko's push and pull flags become per-registry sets (ADR-0036).
    #[test]
    fn insecure_and_tls_flags_apply_to_their_direction() {
        let df =
            rnk_dockerfile::parse("FROM pull.example/base:1\nCOPY --from=copy.example/x:1 /a /a\n")
                .unwrap();
        let graph = rnk_dockerfile::stage_graph(&df, &GraphOptions::default()).unwrap();
        let o = client_options(
            &config_of(&[
                "-d",
                "push.example/out:1",
                "--insecure",
                "--skip-tls-verify-pull",
            ]),
            &graph,
        )
        .unwrap();
        assert_eq!(
            o.insecure.iter().collect::<Vec<_>>(),
            ["push.example"],
            "--insecure is for destinations"
        );
        assert_eq!(
            o.skip_tls_verify.iter().collect::<Vec<_>>(),
            ["copy.example", "pull.example"]
        );
        assert!(o.push_retry.is_none() && o.pull_retry.is_none());
        let o = client_options(
            &config_of(&[
                "-d",
                "push.example/out:1",
                "--insecure-pull",
                "--skip-tls-verify",
                "--skip-tls-verify-registry=other.example",
                "--push-retry=2",
                "--image-download-retry=0",
            ]),
            &graph,
        )
        .unwrap();
        assert_eq!(
            o.insecure.iter().collect::<Vec<_>>(),
            ["copy.example", "pull.example"]
        );
        assert_eq!(
            o.skip_tls_verify.iter().collect::<Vec<_>>(),
            ["other.example", "push.example"]
        );
        assert_eq!(o.push_retry, Some(RetryPolicy::kaniko(2)));
        assert_eq!(o.pull_retry.map(|p| p.attempts), Some(1));
    }

    /// kaniko's formats (`push.go:194-256`): the digest without a newline;
    /// one `repository[:tag]@digest` line per destination, Docker Hub as
    /// `index.docker.io`, and the tagged text in both files when the tag
    /// file is asked for.
    #[test]
    fn digest_files_are_written_as_kaniko_writes_them() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n).display().to_string();
        let digest = "sha256:0123";
        let (d, n) = (p("digest"), p("names"));
        let c = config_of(&[
            "-d",
            "alpine",
            "-d",
            "r.example:5000/team/app:v1",
            "--digest-file",
            &d,
            "--image-name-with-digest-file",
            &n,
        ]);
        write_digest_files(&c, digest).unwrap();
        assert_eq!(std::fs::read_to_string(&d).unwrap(), digest);
        assert_eq!(
            std::fs::read_to_string(&n).unwrap(),
            "index.docker.io/library/alpine@sha256:0123\n\
             r.example:5000/team/app@sha256:0123\n"
        );
        let t = p("tags");
        let c = config_of(&[
            "-d",
            "alpine",
            "--image-name-with-digest-file",
            &n,
            "--image-name-tag-with-digest-file",
            &t,
        ]);
        write_digest_files(&c, digest).unwrap();
        let tagged = "index.docker.io/library/alpine:latest@sha256:0123\n";
        assert_eq!(std::fs::read_to_string(&t).unwrap(), tagged);
        assert_eq!(std::fs::read_to_string(&n).unwrap(), tagged);
    }

    #[test]
    fn a_cancel_waits_for_the_pulls_to_end() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let r = rt.block_on(async {
            let work = tokio::task::spawn_blocking(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                flag.store(true, Ordering::SeqCst);
                Ok::<_, Error>(())
            });
            await_pulls(work, &cancel).await
        });
        assert!(
            done.load(Ordering::SeqCst),
            "returned before the pull ended"
        );
        assert!(matches!(r, Err(Error::Cancelled)));
    }
}
