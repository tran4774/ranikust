//! The stage executor without mounts (T0): stages built `FROM scratch` and
//! `FROM <stage>` on a fake snapshotter whose roots are plain directories.
//! `RUN` needs the real sandbox and is tested end to end (`just smoke`,
//! `just test-vm smoke`); here only its refusals, which come first.
//!
//! Every test creates a [`Capture`] before it reaches a span: tracing caches
//! once per process whether a span is wanted.

use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rnk_builder::{
    BuildCx, BuildTime, ContextFiles, Error, LayerDemand, LayerExporter, StageBase, StageDone,
    StageInput, Ungated, build_stage,
};
use rnk_context::ContextSpec;
use rnk_dockerfile::GraphOptions;
use rnk_layer::{Compression, EncodeOptions};
use rnk_metrics::Capture;
use rnk_oci::{Digest, Platform};
use rnk_rootfs::{CopyOpts, RootPath};
use rnk_sandbox::{Network, Sandbox};
use rnk_snapshot::{Base, LayerDiff, LayerDir, Snapshotter, StageRootfs};
use rnk_store::{BuildPaths, Store};

/// Stage roots as plain directories: a layer is the whole root. A
/// `FROM scratch` root starts with the `seed` entries.
struct FakeSnapshotter {
    dir: PathBuf,
    next: AtomicUsize,
    released: Arc<AtomicUsize>,
    seed: Vec<(String, String)>,
}

struct FakeRoot {
    path: PathBuf,
    root: RootPath,
    open: bool,
    released: Arc<AtomicUsize>,
}

/// Creates `entries` under `dir`: a name ending in `/` is a directory,
/// content starting with `->` a symlink to the rest, anything else a file.
fn plant(dir: &Path, entries: &[(&str, &[u8])]) {
    for (p, content) in entries {
        let p = dir.join(p);
        if p.to_string_lossy().ends_with('/') {
            fs::create_dir_all(&p).unwrap();
            continue;
        }
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        match content.strip_prefix(b"->") {
            Some(target) => {
                std::os::unix::fs::symlink(std::str::from_utf8(target).unwrap(), &p).unwrap()
            }
            None => fs::write(p, content).unwrap(),
        }
    }
}

impl Snapshotter for FakeSnapshotter {
    type Root = FakeRoot;

    fn base_layer<R: std::io::Read>(
        &self,
        _: &Digest,
        _: R,
        _: &[LayerDir],
    ) -> rnk_snapshot::Result<LayerDir> {
        unreachable!("no image in these tests")
    }

    fn prepare(&self, base: Base<'_, FakeRoot>) -> rnk_snapshot::Result<FakeRoot> {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("stage-{n}"));
        fs::create_dir(&path).unwrap();
        let root = RootPath::open(&path)?;
        match base {
            Base::Stage(parent) => {
                rnk_rootfs::copy(&parent.root, "/", &root, "/", &CopyOpts::default())?;
            }
            Base::Scratch => {
                let seed: Vec<(&str, &[u8])> = self
                    .seed
                    .iter()
                    .map(|(p, c)| (p.as_str(), c.as_bytes()))
                    .collect();
                plant(&path, &seed);
            }
            Base::Image(_) => unreachable!("no image in these tests"),
        }
        Ok(FakeRoot {
            path,
            root,
            open: false,
            released: Arc::clone(&self.released),
        })
    }
}

impl StageRootfs for FakeRoot {
    fn root(&self) -> rnk_snapshot::Result<&RootPath> {
        if self.open {
            Ok(&self.root)
        } else {
            Err(rnk_snapshot::Error::State("no layer open"))
        }
    }

    fn mount_point(&self) -> &Path {
        &self.path
    }

    fn begin_layer(&mut self) -> rnk_snapshot::Result<()> {
        self.open = true;
        Ok(())
    }

    fn end_layer(&mut self) -> rnk_snapshot::Result<LayerDiff> {
        self.open = false;
        Ok(LayerDiff::new(self.path.clone(), Vec::new()))
    }

    fn view(&mut self) -> rnk_snapshot::Result<&RootPath> {
        Ok(&self.root)
    }

    fn release(self) -> rnk_snapshot::Result<()> {
        fs::remove_dir_all(&self.path).unwrap();
        self.released.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Build {
    _dir: tempfile::TempDir,
    stages: Vec<StageDone<FakeRoot>>,
    /// Roots released by the builder itself (a failed stage's).
    released: usize,
}

/// Builds every needed stage of `dockerfile` over a context of `files`
/// (`.dockerignore` among them), stage after stage. `{ID}` in the
/// Dockerfile is replaced by `uid:gid` of the test process, for `--chown`
/// (an unprivileged process cannot give files to root).
fn build(dockerfile: &str, files: &[(&str, &[u8])]) -> (Build, Result<(), Error>) {
    build_seeded(dockerfile, files, &[])
}

/// [`build`] with `FROM scratch` roots that start with `seed` (as
/// [`plant`] reads it).
fn build_seeded(
    dockerfile: &str,
    files: &[(&str, &[u8])],
    seed: &[(&str, &str)],
) -> (Build, Result<(), Error>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = dir.path().join("ctx");
    fs::create_dir_all(&ctx).unwrap();
    plant(&ctx, files);
    let meta = fs::metadata(dir.path()).unwrap();
    let dockerfile = dockerfile.replace("{ID}", &format!("{}:{}", meta.uid(), meta.gid()));

    let context = rnk_context::open(&ContextSpec::Dir(ctx)).unwrap();
    let ignore = context.find_dockerignore(None).unwrap();
    let matcher = ignore.as_ref().map(|i| context.matcher(i).unwrap());
    let context_files = ContextFiles::load(&context, matcher.as_ref()).unwrap();
    let state = dir.path().join("state");
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let sandbox = Sandbox::start(None, &state.join("sandbox")).unwrap();
    let time = BuildTime::reproducible(None).unwrap();
    let exporter = LayerExporter::new(
        store.build_dir(),
        EncodeOptions {
            compression: Compression::Gzip,
            level: 6,
            threads: NonZeroUsize::MIN,
            mtime: None,
        },
        &time,
    );
    let roots = dir.path().join("roots");
    fs::create_dir(&roots).unwrap();
    let released = Arc::new(AtomicUsize::new(0));
    let snapshotter = FakeSnapshotter {
        dir: roots,
        next: AtomicUsize::new(0),
        released: Arc::clone(&released),
        seed: seed
            .iter()
            .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
            .collect(),
    };
    let platform = Platform::new("linux", "amd64");
    let secrets = BTreeMap::new();
    let never = || false;
    let cx = BuildCx {
        snapshotter: &snapshotter,
        sandbox: &sandbox,
        store: &store,
        context: &context,
        context_files: &context_files,
        exporter: &exporter,
        time: &time,
        network: Network::Host,
        secrets: &secrets,
        allow_network_host: false,
        platform: &platform,
        run_gate: &Ungated,
        cancelled: &never,
    };
    let df = rnk_dockerfile::parse(&dockerfile).unwrap();
    let graph = rnk_dockerfile::stage_graph(&df, &GraphOptions::default()).unwrap();
    let mut done: Vec<StageDone<FakeRoot>> = Vec::new();
    let mut by_index: BTreeMap<usize, usize> = BTreeMap::new();
    let mut result = Ok(());
    for &i in &graph.needed {
        let node = &graph.stages[i];
        let base = match node.base {
            rnk_dockerfile::Base::Scratch => StageBase::Scratch,
            rnk_dockerfile::Base::Stage(b) => StageBase::Stage(&done[by_index[&b]]),
            rnk_dockerfile::Base::Image(_) => unreachable!("no image in these tests"),
        };
        let input = StageInput {
            index: i,
            graph: &graph,
            stage: &df.stages[i],
            base,
            stages: node
                .dependencies()
                .iter()
                .map(|d| (*d, &done[by_index[d]]))
                .collect(),
            images: BTreeMap::new(),
            demand: LayerDemand {
                target: true,
                ..LayerDemand::default()
            },
        };
        match build_stage(&cx, input) {
            Ok(d) => {
                by_index.insert(i, done.len());
                done.push(d);
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    drop(sandbox);
    let released = released.load(Ordering::SeqCst);
    (
        Build {
            _dir: dir,
            stages: done,
            released,
        },
        result,
    )
}

/// The fake's directory of the `n`th stage built: named in creation order.
fn root_of(b: &Build, n: usize) -> PathBuf {
    b._dir.path().join("roots").join(format!("stage-{n}"))
}

#[test]
fn metadata_and_copies_build_the_config_and_the_root() {
    let _capture = Capture::new();
    let (b, r) = build(
        "FROM scratch AS base\n\
         ARG V=1\n\
         ENV A=$V B=two\n\
         LABEL l=$A\n\
         WORKDIR /app\n\
         COPY --chown={ID} *.txt ./\n\
         COPY --chown={ID} sub sub2/\n\
         COPY --chown={ID} <<EOF note\n\
         hello $A\n\
         EOF\n\
         EXPOSE 80/udp 81\n\
         ENTRYPOINT [\"/bin/app\"]\n\
         CMD run\n\
         STOPSIGNAL SIGINT\n\
         USER 1234\n\
         FROM base\n\
         COPY --from=base --chown={ID} /app/a.txt /copied/\n",
        &[
            ("a.txt", b"a"),
            ("b.txt", b"b"),
            ("ignored.txt", b"x"),
            ("sub/c.txt", b"c"),
            ("sub/skip.log", b"x"),
            (".dockerignore", b"ignored.txt\nsub/*.log\n"),
        ],
    );
    r.unwrap();
    let c = b.stages[0].config.config.clone().unwrap();
    let env = c.env.unwrap();
    assert!(env.contains(&"A=1".to_owned()), "{env:?}");
    // kaniko's `PATH` for `FROM scratch`.
    assert!(
        env.contains(&format!("PATH={}", rnk_dockerfile::DEFAULT_PATH)),
        "{env:?}"
    );
    assert!(env.contains(&"B=two".to_owned()), "{env:?}");
    assert!(
        !env.iter().any(|e| e.starts_with("V=")),
        "ARG is not in the config"
    );
    assert_eq!(c.labels.unwrap()["l"], "1");
    assert_eq!(c.working_dir.as_deref(), Some("/app"));
    let ports: Vec<_> = c.exposed_ports.unwrap().into_iter().collect();
    assert_eq!(ports, ["80/udp", "81/tcp"]);
    assert_eq!(c.entrypoint.unwrap(), ["/bin/app"]);
    assert_eq!(c.cmd.unwrap(), ["/bin/sh", "-c", "run"]);
    assert_eq!(c.stop_signal.as_deref(), Some("SIGINT"));
    assert_eq!(c.user.as_deref(), Some("1234"));

    let r0 = root_of(&b, 0);
    assert_eq!(fs::read(r0.join("app/a.txt")).unwrap(), b"a");
    assert_eq!(fs::read(r0.join("app/b.txt")).unwrap(), b"b");
    assert!(!r0.join("app/ignored.txt").exists(), ".dockerignore");
    assert_eq!(fs::read(r0.join("app/sub2/c.txt")).unwrap(), b"c");
    assert!(
        !r0.join("app/sub2/skip.log").exists(),
        ".dockerignore in a directory"
    );
    assert_eq!(
        fs::read_to_string(r0.join("app/note")).unwrap(),
        "hello 1\n"
    );

    // WORKDIR made /app, and three COPYs: four layers, one history entry
    // each.
    let config = &b.stages[0].config;
    assert_eq!(config.rootfs.diff_ids.len(), 4);
    assert_eq!(b.stages[0].layers.len(), 4);
    assert_eq!(config.history.as_ref().unwrap().len(), 4);

    // The second stage inherits the first and copies from it.
    let r1 = root_of(&b, 1);
    assert_eq!(fs::read(r1.join("copied/a.txt")).unwrap(), b"a");
    assert_eq!(fs::read(r1.join("app/b.txt")).unwrap(), b"b");
    assert_eq!(b.stages[1].layers.len(), 5);
    assert_eq!(
        b.stages[1].config.config.as_ref().unwrap().user.as_deref(),
        Some("1234")
    );
}

#[test]
fn later_phase_features_and_bad_sources_are_refused_with_a_reason() {
    let _capture = Capture::new();
    let files: &[(&str, &[u8])] = &[
        ("a.txt", b"a"),
        ("b.txt", b"b"),
        ("ignored.txt", b"x"),
        ("archive.tgz", &[0x1f, 0x8b, 8, 0]),
        (".dockerignore", b"ignored.txt\n"),
    ];
    let cases = [
        ("ADD https://example.com/a /a", "Phase 5"),
        ("COPY --link a.txt /", "Phase 5"),
        ("RUN --mount=type=bind,target=/x true", "Phase 5"),
        ("RUN --security=insecure true", "not supported"),
        ("COPY --chown={ID} a.txt b.txt /dest", "must be a directory"),
        ("COPY missing.txt /", "not found"),
        ("COPY ignored.txt /", "not found"),
        ("COPY --chmod=u+x a.txt /", "octal"),
        ("ADD --chown={ID} archive.tgz /", "Phase 5"),
        ("ONBUILD RUN true\nFROM p", "ONBUILD"),
    ];
    for (instruction, want) in cases {
        let (_b, r) = build(&format!("FROM scratch AS p\n{instruction}\n"), files);
        let e = r.expect_err(instruction).to_string();
        assert!(e.contains(want), "{instruction}: {e}");
    }
}

#[test]
fn a_stage_without_instructions_has_no_layer_of_its_own() {
    let _capture = Capture::new();
    let (b, r) = build("FROM scratch\n", &[]);
    r.unwrap();
    assert!(b.stages[0].layers.is_empty());
    assert!(b.stages[0].config.rootfs.diff_ids.is_empty());
}

#[test]
fn stage_and_instruction_spans_are_recorded() {
    let capture = Capture::new();
    let (_b, r) = capture.run(|| {
        build(
            "FROM scratch\nENV A=1\nCOPY --chown={ID} a.txt /\n",
            &[("a.txt", b"a")],
        )
    });
    r.unwrap();
    let report = capture.report();
    assert_eq!(
        report["phases"]["rnk_builder.stage"]["count"], 1,
        "{report:#}"
    );
    assert_eq!(report["phases"]["rnk_builder.layer"]["count"], 1);
    let commands = report["commands"].as_array().unwrap();
    let kinds: Vec<_> = commands
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["ENV", "COPY"], "{commands:?}");
}

#[test]
fn a_failed_stage_releases_its_root() {
    let _capture = Capture::new();
    let (b, r) = build(
        "FROM scratch\nENV A=1\nCOPY missing /\n",
        &[("a.txt", b"a")],
    );
    assert!(r.is_err());
    assert!(b.stages.is_empty());
    assert_eq!(b.released, 1);
}

#[test]
fn symlinked_sources_cannot_reach_ignored_files() {
    let _capture = Capture::new();
    let files: &[(&str, &[u8])] = &[
        ("a.txt", b"a"),
        (".env", b"SECRET"),
        ("secrets/key", b"SECRET"),
        ("to-env", b"->.env"),
        ("to-secrets", b"->secrets"),
        ("to-a", b"->a.txt"),
        ("dir/", b""),
        ("dir/up", b"->../secrets"),
        (".dockerignore", b".env\nsecrets\n"),
    ];
    for source in ["to-env", "to-secrets", "to-secrets/key", "dir/up"] {
        let (_b, r) = build(
            &format!("FROM scratch\nCOPY --chown={{ID}} {source} /x\n"),
            files,
        );
        let e = r.expect_err(source).to_string();
        assert!(e.contains("not found"), "{source}: {e}");
    }
    // A directory copied whole leaves its symlinks as links, unread.
    let (b, r) = build("FROM scratch\nCOPY --chown={ID} dir /d\n", files);
    r.unwrap();
    let up = root_of(&b, 0).join("d/up");
    assert_eq!(fs::read_link(up).unwrap(), PathBuf::from("../secrets"));
    // A kept link to a kept file copies the file, under the link's name.
    let (b, r) = build("FROM scratch\nCOPY --chown={ID} to-a /dst/\n", files);
    r.unwrap();
    assert_eq!(fs::read(root_of(&b, 0).join("dst/to-a")).unwrap(), b"a");
}

#[test]
fn a_copy_onto_a_symlink_to_a_directory_goes_inside_it() {
    let _capture = Capture::new();
    let (b, r) = build_seeded(
        "FROM scratch\nCOPY --chown={ID} a.txt /link\n",
        &[("a.txt", b"a")],
        &[("real/", ""), ("link", "->real")],
    );
    r.unwrap();
    let root = root_of(&b, 0);
    assert_eq!(fs::read(root.join("real/a.txt")).unwrap(), b"a");
    assert_eq!(
        fs::read_link(root.join("link")).unwrap(),
        PathBuf::from("real")
    );
}
