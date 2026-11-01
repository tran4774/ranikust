//! Red-team tests of `COPY --from` in sequential kaniko mode (T0,
//! unprivileged): stages built on the real kaniko-mode snapshotter over one
//! shared root, a temp directory, each later unit reading an earlier stage
//! from its kept copy (`SourceRoot::keep`), as `ranikust`'s `run_unit`
//! keeps it.
//!
//! The first stage's root is planted with what a hostile `RUN` could leave
//! (links out of the root, into the ignored state, loops, a FIFO, a
//! socket, setuid and setgid bits, hardlinks, an xattr); the root holds
//! `state/blob` on the ignore list, as the state directory sits on `/`, and
//! `outside/secret` lies beside it. A later `COPY --from` must read exactly
//! what it would have read from the stage, never the state or the host
//! (threat model T1, T4).

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::num::NonZeroUsize;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use rnk_builder::{
    BuildCx, BuildTime, ContextFiles, KeepNeed, LayerDemand, LayerExporter, StageBase, StageDone,
    StageInput, Ungated, build_stage,
};
use rnk_context::ContextSpec;
use rnk_dockerfile::GraphOptions;
use rnk_layer::{Compression, EncodeOptions};
use rnk_oci::Platform;
use rnk_rootfs::{IgnoreList, RootPath};
use rnk_sandbox::{Network, Sandbox};
use rnk_snapshot::{Kaniko, KanikoRoot, WalkOptions};
use rnk_store::{BuildPaths, Store};

const HOST: &[u8] = b"HOST-SECRET";
const STATE: &[u8] = b"STATE-SECRET";

/// What a build left: its result, and the root's `out` tree.
struct Outcome {
    result: Result<(), String>,
    /// `out` below the root after the last stage, if it exists.
    out: Option<Vec<(String, Entry)>>,
    /// The first stage's `app` as planted, before it was kept.
    app: Vec<(String, Entry)>,
    /// The last stage's diff IDs and, per history entry, its text and
    /// whether it is an empty layer.
    image: (Vec<String>, Vec<(String, bool)>),
}

/// Builds the needed stages of `dockerfile` in order on one shared root,
/// as sequential kaniko mode does: after stage 0, `plant` fills its root
/// and `need` keeps it (or not), and every later stage starts on an
/// emptied root. Checks that nothing outside the root, nor the ignored
/// state, was touched or copied.
fn run(dockerfile: &str, need: Option<KeepNeed>) -> Outcome {
    run_in(dockerfile, need, false)
}

/// [`run`]; with `tars`, as on `/` (ADR-0056): an anonymous store in the
/// state directory inside the root, stages kept as tars in its anonymous
/// files, a reader's view unpacked in the state directory.
fn run_in(dockerfile: &str, need: Option<KeepNeed>, tars: bool) -> Outcome {
    let tmp = tempfile::tempdir().unwrap();
    let top = fs::canonicalize(tmp.path()).unwrap();
    let root = top.join("root");
    let outside = top.join("outside");
    fs::create_dir_all(root.join("state")).unwrap();
    fs::write(root.join("state/blob"), STATE).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), HOST).unwrap();
    fs::create_dir(top.join("kept")).unwrap();
    let ctx = top.join("ctx");
    fs::create_dir(&ctx).unwrap();

    let mut ignore = IgnoreList::new();
    ignore.insert("/state").unwrap();
    let kept = if tars {
        root.join("state")
    } else {
        top.join("kept")
    };
    let kaniko = Kaniko::new(
        RootPath::open(&root).unwrap(),
        root.clone(),
        ignore,
        WalkOptions::default(),
        RootPath::open(kept).unwrap(),
    );
    let context = rnk_context::open(&ContextSpec::Dir(ctx)).unwrap();
    let context_files = ContextFiles::load(&context, None).unwrap();
    let state = top.join("store");
    let store = Arc::new(if tars {
        Store::open_anonymous(&root.join("state")).unwrap()
    } else {
        Store::open(&BuildPaths::new(&top, &state)).unwrap()
    });
    if tars {
        kaniko.keep_in_anonymous_files(Arc::clone(&store)).unwrap();
    }
    let sandbox = Sandbox::start_direct().unwrap();
    let time = BuildTime::reproducible(None).unwrap();
    let options = EncodeOptions {
        compression: Compression::Gzip,
        level: 1,
        threads: NonZeroUsize::MIN,
        mtime: None,
    };
    let exporter = if tars {
        LayerExporter::anonymous(Arc::clone(&store), options, &time)
    } else {
        LayerExporter::new(store.build_dir(), options, &time)
    };
    let platform = Platform::new("linux", "amd64");
    let secrets = BTreeMap::new();
    let never = || false;
    let cx = BuildCx {
        isolation: rnk_sandbox::Isolation::Full,
        snapshotter: &kaniko,
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
    let df = rnk_dockerfile::parse(dockerfile).unwrap();
    let graph = rnk_dockerfile::stage_graph(&df, &GraphOptions::default()).unwrap();
    let mut done: Vec<StageDone<KanikoRoot>> = Vec::new();
    let mut by_index: BTreeMap<usize, usize> = BTreeMap::new();
    let mut result = Ok(());
    let mut app = Vec::new();
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
                target: i == graph.primary_target(),
                ..LayerDemand::default()
            },
            labels: &[],
        };
        let built = within_time(|| build_stage(&cx, input));
        match built {
            Ok(d) => {
                if i == 0 {
                    plant_hostile(&root, &outside);
                    app = tree(&root.join("app"));
                    let kept = match &need {
                        Some(n) => within_time(|| d.root.keep(n)),
                        None => d.root.release(),
                    };
                    if let Err(e) = kept {
                        result = Err(format!("keep: {e}"));
                        break;
                    }
                }
                by_index.insert(i, done.len());
                done.push(d);
            }
            Err(e) => {
                result = Err(e.to_string());
                break;
            }
        }
    }
    // Nothing outside the root changed; the state is intact and was never
    // copied anywhere.
    assert_eq!(fs::read(outside.join("secret")).unwrap(), HOST);
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    assert_eq!(fs::read(root.join("state/blob")).unwrap(), STATE);
    assert_eq!(state_names(&root), ["blob"], "{dockerfile}");
    for dir in [top.join("kept"), root.clone()] {
        for f in files_below(&dir, &root.join("state")) {
            let data = fs::read(&f).unwrap();
            assert!(
                data != HOST && data != STATE,
                "{dockerfile}: a secret was copied to {}",
                f.display()
            );
        }
    }
    let out = root
        .join("out")
        .symlink_metadata()
        .is_ok()
        .then(|| tree(&root.join("out")));
    let image = done.last().map_or_else(Default::default, |d| {
        let history = d.config.history.as_deref().unwrap_or_default();
        (
            d.config
                .rootfs
                .diff_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
            history
                .iter()
                .map(|h| {
                    (
                        h.created_by.clone().unwrap_or_default(),
                        h.empty_layer == Some(true),
                    )
                })
                .collect(),
        )
    });
    for d in done.into_iter().rev() {
        d.root.release().unwrap();
    }
    assert_eq!(
        state_names(&root),
        ["blob"],
        "an unpacked view outlived its stage"
    );
    assert_eq!(
        fs::read_dir(top.join("kept")).unwrap().count(),
        0,
        "a kept copy outlived its stage"
    );
    Outcome {
        result,
        out,
        app,
        image,
    }
}

/// The names in the state directory but the anonymous store's marker of a
/// build in progress.
fn state_names(root: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(root.join("state"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != ".ranikust-building")
        .collect();
    v.sort();
    v
}

/// Runs `f` on another thread, failing after 30 s: a FIFO opened for
/// reading would block forever.
fn within_time<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        s.spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("blocked: a FIFO or device was opened")
    })
}

/// What a `RUN` of a hostile stage could leave in its root.
fn plant_hostile(r: &Path, outside: &Path) {
    use std::os::unix::fs::symlink;
    symlink(outside, r.join("host")).unwrap();
    symlink("../../outside", r.join("climb")).unwrap();
    symlink("/state", r.join("to-state")).unwrap();
    symlink("../state/blob", r.join("to-blob")).unwrap();
    symlink("/", r.join("to-root")).unwrap();
    symlink("loop", r.join("loop")).unwrap();
    symlink("/app/dist", r.join("dist")).unwrap();
    fs::create_dir_all(r.join("app/dist/sub")).unwrap();
    fs::write(r.join("app/dist/main.js"), b"js").unwrap();
    fs::write(r.join("app/dist/main.css"), b"css").unwrap();
    fs::write(r.join("app/dist/sub/x"), b"x").unwrap();
    symlink("../../state", r.join("app/dist/state")).unwrap();
    symlink("/to-root/state", r.join("app/abs-state")).unwrap();
    fs::hard_link(r.join("app/dist/main.js"), r.join("app/main-link.js")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        r.join("app/fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o640),
        0,
    )
    .unwrap();
    drop(std::os::unix::net::UnixListener::bind(r.join("app/sock")).unwrap());
    fs::write(r.join("app/suid"), b"#!/bin/sh\n").unwrap();
    fs::set_permissions(r.join("app/suid"), fs::Permissions::from_mode(0o4755)).unwrap();
    fs::create_dir(r.join("app/sgid")).unwrap();
    fs::set_permissions(r.join("app/sgid"), fs::Permissions::from_mode(0o2775)).unwrap();
    fs::create_dir(r.join("app/private")).unwrap();
    fs::write(r.join("app/private/key"), b"k").unwrap();
    fs::set_permissions(r.join("app/private"), fs::Permissions::from_mode(0o700)).unwrap();
    let _ = rustix::fs::setxattr(
        r.join("app/dist/main.js"),
        "user.rnk",
        b"v",
        rustix::fs::XattrFlags::empty(),
    );
}

/// Every regular file below `dir` but `skip`, never following a symlink.
fn files_below(dir: &Path, skip: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        if d == skip {
            continue;
        }
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd {
            let e = e.unwrap();
            let t = e.file_type().unwrap();
            if t.is_dir() {
                todo.push(e.path());
            } else if t.is_file() {
                out.push(e.path());
            }
        }
    }
    out
}

/// An entry as a copy should give it back: type, mode, owner, mtime, link
/// target, content, xattr, and the names sharing its inode.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    kind: &'static str,
    mode: u32,
    owner: (u32, u32),
    mtime: (i64, i64),
    target: Option<PathBuf>,
    data: Option<Vec<u8>>,
    xattr: Option<Vec<u8>>,
    links: Vec<String>,
}

/// The entries below `dir`, by relative name.
fn tree(dir: &Path) -> Vec<(String, Entry)> {
    let mut out = Vec::new();
    let mut by_ino: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        for e in fs::read_dir(dir.join(&rel)).unwrap() {
            let e = e.unwrap();
            let name = rel.join(e.file_name());
            let shown = name.to_string_lossy().into_owned();
            let full = dir.join(&name);
            let m = fs::symlink_metadata(&full).unwrap();
            let t = m.file_type();
            let kind = if t.is_dir() {
                todo.push(name.clone());
                "dir"
            } else if t.is_symlink() {
                "link"
            } else if t.is_fifo() {
                "fifo"
            } else if t.is_socket() {
                "socket"
            } else {
                "file"
            };
            if t.is_file() && m.nlink() > 1 {
                by_ino.entry(m.ino()).or_default().push(shown.clone());
            }
            let mut buf = vec![0; 16];
            let xattr = rustix::fs::lgetxattr(&full, "user.rnk", &mut buf)
                .ok()
                .map(|n| buf[..n].to_vec());
            out.push((
                shown,
                Entry {
                    kind,
                    mode: m.mode() & 0o7777,
                    owner: (m.uid(), m.gid()),
                    mtime: (m.mtime(), m.mtime_nsec()),
                    target: t.is_symlink().then(|| fs::read_link(&full).unwrap()),
                    data: t.is_file().then(|| {
                        let mut v = Vec::new();
                        fs::File::open(&full).unwrap().read_to_end(&mut v).unwrap();
                        v
                    }),
                    xattr,
                    links: Vec::new(),
                },
            ));
        }
    }
    for (name, e) in &mut out {
        if let Some(group) = by_ino.values().find(|g| g.contains(name)) {
            e.links = group.clone();
            e.links.sort();
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn sources(list: &[&str]) -> Option<KeepNeed> {
    Some(KeepNeed::Sources(
        list.iter().map(|s| (*s).to_owned()).collect(),
    ))
}

fn copy_from(sources: &str) -> String {
    format!("FROM scratch AS a\nFROM scratch\nCOPY --from=a {sources} /out/\n")
}

#[test]
fn copy_from_through_links_never_reads_the_state_or_the_host() {
    // Each source names the ignored state or the host through a link, or
    // climbs out with `..`: read from the kept copy, it is not found,
    // whether the sources or the whole stage were kept.
    for source in [
        "/to-state/blob",
        "/to-blob",
        "/to-root/state/blob",
        "/to-root/state",
        "/app/dist/state/blob",
        "/app/dist/state",
        "/app/abs-state/blob",
        "/dist/state/blob",
        "/state/blob",
        "/state",
        "/host/secret",
        "/climb/secret",
        "/../../outside/secret",
        "/loop/x",
    ] {
        for need in [sources(&[source]), Some(KeepNeed::All)] {
            let o = run(&copy_from(source), need.clone());
            assert!(
                o.result.is_err(),
                "{source} ({need:?}): copied {:?}",
                o.out
                    .map(|t| t.into_iter().map(|e| e.0).collect::<Vec<_>>())
            );
        }
    }
}

#[test]
fn copy_from_wildcards_and_the_whole_root_leave_the_state_out() {
    for source in [
        "/",
        "/.",
        "/to-root",
        "/to-root/",
        "/t*",
        "/*/state",
        "/app/*",
    ] {
        for need in [sources(&[source]), Some(KeepNeed::All)] {
            let o = run(&copy_from(source), need.clone());
            // Links named `state` are copied as links; the state's
            // directory and its file never are (`run` checks every file's
            // content too).
            if let Some(out) = &o.out {
                for (name, e) in out {
                    assert!(
                        e.kind == "link" || !(name.ends_with("blob") || name.ends_with("state")),
                        "{source} ({need:?}): {name} copied"
                    );
                }
            }
            if let Err(e) = &o.result {
                eprintln!("{source} ({need:?}): {e}");
            }
        }
    }
}

/// `COPY --from=a /app /out/` copies `app`'s contents: compared with `app`
/// as planted, but for the socket, which a copy turns into a file.
fn same_as_app(o: &Outcome, what: &str) {
    let out = o.out.as_ref().unwrap_or_else(|| panic!("{what}: no /out"));
    let want: Vec<_> = o.app.iter().filter(|e| e.0 != "sock").cloned().collect();
    let got: Vec<_> = out.iter().filter(|e| e.0 != "sock").cloned().collect();
    assert_eq!(
        want.iter().map(|e| &e.0).collect::<Vec<_>>(),
        got.iter().map(|e| &e.0).collect::<Vec<_>>(),
        "{what}: names"
    );
    for (w, g) in want.iter().zip(&got) {
        assert_eq!(w, g, "{what}: {} differs from the stage's", w.0);
    }
}

#[test]
fn a_kept_directory_is_copied_as_the_stage_had_it() {
    for need in [sources(&["/app"]), Some(KeepNeed::All)] {
        let o = run(&copy_from("/app"), need.clone());
        o.result.as_ref().unwrap();
        same_as_app(&o, &format!("{need:?}"));
    }
    // Through a link on the way: the same entries.
    let o = run(
        "FROM scratch AS a\nFROM scratch\nCOPY --from=a /to-root/app /out/\n",
        sources(&["/to-root/app"]),
    );
    o.result.as_ref().unwrap();
    same_as_app(&o, "/to-root/app");
}

#[test]
fn overlapping_sources_are_kept_in_either_order() {
    for list in [
        ["/app/dist/main.js", "/app"],
        ["/app", "/app/dist/main.js"],
        ["/dist/main.js", "/app"],
        ["/app/dist", "/dist"],
        ["/dist", "/app/dist"],
    ] {
        let df = format!(
            "FROM scratch AS a\nFROM scratch\nCOPY --from=a {} /out/\nCOPY --from=a {} /out/\n",
            list[0], list[1]
        );
        let o = run(&df, sources(&list));
        assert!(o.result.is_ok(), "{list:?}: {:?}", o.result);
        let all = run(&df, Some(KeepNeed::All));
        assert_eq!(
            o.out
                .map(|t| t.into_iter().map(|e| (e.0, e.1.data)).collect::<Vec<_>>()),
            all.out
                .map(|t| t.into_iter().map(|e| (e.0, e.1.data)).collect::<Vec<_>>()),
            "{list:?}: kept by sources differs from kept whole"
        );
    }
}

#[test]
fn a_fifo_source_is_copied_without_blocking() {
    for need in [sources(&["/app/fifo"]), Some(KeepNeed::All)] {
        let o = run(
            "FROM scratch AS a\nFROM scratch\nCOPY --from=a /app/fifo /out/fifo\n",
            need.clone(),
        );
        o.result.as_ref().unwrap();
        let out = o.out.unwrap();
        assert_eq!(out[0].0, "fifo", "{need:?}");
        assert_eq!(out[0].1.kind, "fifo", "{need:?}");
    }
}

#[test]
fn an_empty_copy_from_source_is_refused() {
    let o = run(
        "FROM scratch AS a\nFROM scratch\nARG E\nCOPY --from=a $E /out/\n",
        Some(KeepNeed::All),
    );
    let e = o.result.unwrap_err();
    assert!(e.contains("empty"), "{e}");
}

#[test]
fn as_tars_on_slash_copy_from_reads_what_the_stage_had_and_never_the_state() {
    // ADR-0056: the same reads through a stage kept as a tar and a view
    // unpacked in the state directory, asked for before the reading
    // instruction began.
    // A tar carries whole seconds and only `security.capability`, as every
    // layer does (Q-025): the final image is the same; the reading stage
    // sees mtimes without nanoseconds and no other xattr.
    for need in [sources(&["/app"]), Some(KeepNeed::All)] {
        let mut o = run_in(&copy_from("/app"), need.clone(), true);
        for (_, e) in o.app.iter_mut().chain(o.out.iter_mut().flatten()) {
            e.mtime.1 = 0;
            e.xattr = None;
        }
        same_as_app(&o, "/app");
    }
    for source in ["/", "/.", "/to-root/"] {
        for need in [sources(&[source]), Some(KeepNeed::All)] {
            let o = run_in(&copy_from(source), need.clone(), true);
            o.result.as_ref().unwrap();
            let out = o.out.unwrap();
            assert!(
                out.iter().all(|(n, _)| !n.starts_with("state")),
                "{source}: the state was copied"
            );
            assert!(out.iter().any(|(n, _)| n.starts_with("app")), "{source}");
        }
    }
    for source in [
        "/to-state/blob",
        "/to-root/state/blob",
        "/state",
        "/host/secret",
    ] {
        for need in [sources(&[source]), Some(KeepNeed::All)] {
            let o = run_in(&copy_from(source), need.clone(), true);
            assert!(o.result.is_err(), "{source} ({need:?})");
        }
    }
}

/// ADR-0058 (Q-385): on the kaniko-mode walk, a step that changes nothing
/// (a `WORKDIR` of an existing directory, a `COPY` matching nothing) adds
/// no layer, only its history entry with `empty_layer: true`, as kaniko;
/// the image is the same from one build to the next.
#[test]
fn a_step_that_changes_nothing_adds_no_layer() {
    let dockerfile = "FROM scratch\n\
         WORKDIR /app\n\
         WORKDIR /app\n\
         COPY *.nomatch /app/\n\
         WORKDIR /out\n\
         WORKDIR /app\n";
    let build = || {
        let o = run(dockerfile, None);
        o.result.unwrap();
        o.image
    };
    let (diff_ids, history) = build();
    assert_eq!(
        diff_ids.len(),
        2,
        "WORKDIR /app and WORKDIR /out: {history:#?}"
    );
    let empty: Vec<bool> = history.iter().map(|h| h.1).collect();
    assert_eq!(empty, [false, true, true, false, true], "{history:#?}");
    assert_eq!(history[1].0, "WORKDIR /app");
    assert_eq!(build(), (diff_ids, history), "the same image again");
}
