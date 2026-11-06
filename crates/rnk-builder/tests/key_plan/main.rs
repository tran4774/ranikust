//! The key plan (ADR-0113, ADR-0114): what changes a step's key, what never
//! does, and how the context is read for it.

mod support;

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use rnk_cache::{CacheKey, KEY_VERSION};
use rnk_oci::Digest;
use rnk_sandbox::{Isolation, Network};

use crate::support::{Opts, context, encoding, keys, last};

/// Two stages: a `COPY` of a directory, `RUN` mounts, a `COPY --from` of
/// the first and a heredoc.
const BUILD: &str = r#"FROM base AS deps
ARG VERSION=1
ENV MODE=release
WORKDIR /app
COPY src/ ./src/
RUN --mount=type=cache,target=/root/.cache --mount=type=secret,id=token make
FROM scratch
COPY --from=deps /app /app
COPY <<EOF /note
built
EOF
"#;

fn files() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("src/a.txt", b"hello"),
        ("src/b/c.txt", b"world"),
        ("src/run.sh", b"#!/bin/sh\n"),
        ("src/link", b"->a.txt"),
        ("src/empty/", b""),
        (".dockerignore", b"secret.txt\n"),
        ("secret.txt", b"not read"),
    ]
}

#[test]
fn every_step_of_every_stage_gets_a_key() {
    let ctx = context(&files());
    let keys = keys(BUILD, &ctx.path, &Opts::default());
    for (i, stage) in keys.iter().enumerate() {
        let stage = stage
            .as_ref()
            .unwrap_or_else(|| panic!("stage {i} has keys"));
        assert!(stage.steps().iter().all(Option::is_some), "{i}: {stage:?}");
    }
    let deps = keys[0].as_ref().unwrap();
    let out = keys[1].as_ref().unwrap();
    assert_eq!(out.steps()[0].unwrap().reads_stage, Some(0));
    assert_eq!(out.steps()[1].unwrap().reads_stage, None);
    assert_ne!(deps.final_key(), out.final_key());
}

#[test]
fn the_same_build_gives_the_same_keys_on_any_number_of_threads() {
    let ctx = context(&files());
    let one = keys(BUILD, &ctx.path, &Opts::default());
    assert_eq!(one, keys(BUILD, &ctx.path, &Opts::default()));
    let many = Opts {
        threads: 8,
        ..Opts::default()
    };
    assert_eq!(one, keys(BUILD, &ctx.path, &many));
}

#[test]
fn listing_the_context_in_another_order_changes_no_key() {
    let forward = context(&files());
    let mut reversed = files();
    reversed.reverse();
    let backward = context(&reversed);
    let opts = Opts {
        threads: 3,
        ..Opts::default()
    };
    assert_eq!(
        keys(BUILD, &forward.path, &Opts::default()),
        keys(BUILD, &backward.path, &opts)
    );
}

#[test]
fn touching_the_context_changes_no_key() {
    let ctx = context(&files());
    let before = keys(BUILD, &ctx.path, &Opts::default());
    ctx.touch_all(1_000_000);
    assert_eq!(before, keys(BUILD, &ctx.path, &Opts::default()));
    // Under --reproducible, any mtime past the epoch is clamped to it.
    let reproducible = Opts {
        epoch: Some(10),
        ..Opts::default()
    };
    let clamped = keys(BUILD, &ctx.path, &reproducible);
    ctx.touch_all(2_000_000);
    assert_eq!(clamped, keys(BUILD, &ctx.path, &reproducible));
}

#[test]
fn under_reproducible_an_mtime_older_than_the_epoch_counts() {
    let ctx = context(&files());
    let reproducible = Opts {
        epoch: Some(5_000_000),
        ..Opts::default()
    };
    ctx.touch_all(1_000_000);
    let before = last(BUILD, &ctx.path, &reproducible);
    ctx.touch_all(1_000_001);
    assert_ne!(before, last(BUILD, &ctx.path, &reproducible));
}

/// Asserts that `change` gives the last stage another final key.
#[track_caller]
fn assert_changes(
    what: &str,
    a: (&str, &[(&str, &[u8])], Opts),
    b: (&str, &[(&str, &[u8])], Opts),
) {
    let (ca, cb) = (context(a.1), context(b.1));
    assert_ne!(
        last(a.0, &ca.path, &a.2),
        last(b.0, &cb.path, &b.2),
        "{what}"
    );
}

#[test]
fn each_input_changes_the_key() {
    let files = files();
    let base = || (BUILD, &files[..], Opts::default());
    let declared = Opts {
        build_args: vec![("VERSION", "2")],
        ..Opts::default()
    };
    assert_changes("a declared build arg", base(), (BUILD, &files, declared));
    let env = BUILD.replace("MODE=release", "MODE=debug");
    assert_changes("an ENV", base(), (&env, &files, Opts::default()));
    let mut content = files.clone();
    content[0] = ("src/a.txt", b"hellO".as_slice());
    assert_changes(
        "a file's content",
        base(),
        (BUILD, &content, Opts::default()),
    );
    let renamed: Vec<_> = files
        .iter()
        .map(|&(p, c)| {
            if p == "src/a.txt" {
                ("src/A.txt", c)
            } else {
                (p, c)
            }
        })
        .collect();
    assert_changes("a file's name", base(), (BUILD, &renamed, Opts::default()));
    for (what, from, to) in [
        (
            "a cache target",
            "target=/root/.cache",
            "target=/root/.cache2",
        ),
        ("a cache id", "type=cache,", "type=cache,id=other,"),
        (
            "a cache's sharing",
            "type=cache,",
            "type=cache,sharing=locked,",
        ),
        ("a secret id", "id=token", "id=other"),
        ("a secret target", "id=token", "id=token,target=/t"),
        ("a secret mode", "id=token", "id=token,mode=0444"),
        ("a RUN's network", "RUN ", "RUN --network=none "),
        ("a COPY --from source stage's RUN", " make", " make all"),
        ("a heredoc's body", "built", "built twice"),
    ] {
        let changed = BUILD.replace(from, to);
        assert_ne!(changed, BUILD, "{what}");
        assert_changes(what, base(), (&changed, &files, Opts::default()));
    }
}

#[test]
fn a_file_mode_and_chmod_change_the_key() {
    let df = "FROM base\nCOPY run.sh /bin/\n";
    let files: [(&str, &[u8]); 1] = [("run.sh", b"#!/bin/sh\n")];
    let ca = context(&files);
    let cb = context(&files);
    cb.chmod("run.sh", 0o755);
    let opts = Opts::default();
    assert_ne!(last(df, &ca.path, &opts), last(df, &cb.path, &opts), "mode");
    let chmod = |m: &str| format!("FROM base\nCOPY --chmod={m} run.sh /bin/\n");
    assert_ne!(
        last(&chmod("755"), &ca.path, &opts),
        last(&chmod("700"), &ca.path, &opts),
        "--chmod"
    );
}

#[test]
fn a_build_arg_no_arg_declares_changes_no_key() {
    let ctx = context(&files());
    let unused = Opts {
        build_args: vec![("UNUSED", "1")],
        ..Opts::default()
    };
    let other = Opts {
        build_args: vec![("UNUSED", "2")],
        ..Opts::default()
    };
    let none = keys(BUILD, &ctx.path, &Opts::default());
    assert_eq!(none, keys(BUILD, &ctx.path, &unused));
    assert_eq!(none, keys(BUILD, &ctx.path, &other));
}

/// The plan takes no secret value at all; a mount's id is keyed, in the key
/// and in its tag.
#[test]
fn a_secret_id_changes_the_key_and_the_tag() {
    let ctx = context(&[]);
    let df = |id: &str| format!("FROM base\nRUN --mount=type=secret,id={id},env=T true\n");
    let opts = Opts::default();
    let (a, b) = (
        last(&df("a"), &ctx.path, &opts),
        last(&df("b"), &ctx.path, &opts),
    );
    assert_ne!(a, b);
    assert_ne!(a.tag(&encoding(None)), b.tag(&encoding(None)));
}

#[test]
fn a_direct_run_dropping_a_cache_mount_changes_the_key() {
    let ctx = context(&files());
    let direct = Opts {
        isolation: Isolation::Direct,
        ..Opts::default()
    };
    // `BUILD` mounts a secret as a file, which a direct RUN refuses.
    let df = BUILD.replace("type=secret,id=token", "type=secret,id=token,env=TOKEN");
    assert_ne!(
        last(&df, &ctx.path, &Opts::default()),
        last(&df, &ctx.path, &direct)
    );
    // Without a cache mount or special file, the isolation is in no key.
    let plain = "FROM base\nCOPY src/ /src/\nRUN make\n";
    assert_eq!(
        last(plain, &ctx.path, &Opts::default()),
        last(plain, &ctx.path, &direct)
    );
}

#[test]
fn the_build_network_none_is_keyed_for_a_run_without_network() {
    let ctx = context(&[]);
    let df = "FROM base\nRUN true\n";
    let none = Opts {
        network: Network::None,
        ..Opts::default()
    };
    assert_ne!(
        last(df, &ctx.path, &Opts::default()),
        last(df, &ctx.path, &none)
    );
    // A RUN that asks for none runs with none either way.
    let asks = "FROM base\nRUN --network=none true\n";
    assert_eq!(
        last(asks, &ctx.path, &Opts::default()),
        last(asks, &ctx.path, &none)
    );
}

/// Content `|fifo`: hashing would block on opening it.
#[test]
fn a_fifo_is_keyed_without_being_opened() {
    let ctx = context(&[("a.txt", b"a"), ("pipe", b"|fifo")]);
    let df = "FROM base\nCOPY . /\n";
    let (tx, rx) = mpsc::channel();
    let path = ctx.path.clone();
    let planner = std::thread::spawn(move || {
        // The receiver is gone only once the test has failed.
        tx.send(last(df, &path, &Opts::default())).ok();
    });
    let fifo = match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(key) => key,
        Err(RecvTimeoutError::Timeout) => {
            // A writer unblocks a plan blocked in its open, so the thread
            // and the context go before the test fails; a plan blocked
            // elsewhere is left behind rather than waited for.
            use rustix::fs::{Mode, OFlags, open};
            let writer = open(
                ctx.file("pipe"),
                OFlags::WRONLY | OFlags::NONBLOCK,
                Mode::empty(),
            );
            if writer.is_ok() {
                drop(writer);
                planner.join().ok();
            }
            panic!("the plan opened the FIFO");
        }
        Err(RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(planner.join().unwrap_err());
        }
    };
    planner.join().unwrap();
    let file = context(&[("a.txt", b"a"), ("pipe", b"")]);
    assert_ne!(
        fifo,
        last(df, &file.path, &Opts::default()),
        "a FIFO is not an empty file"
    );
}

#[test]
fn a_socket_is_keyed_as_the_empty_file_a_copy_makes_of_it() {
    let df = "FROM base\nCOPY . /\n";
    let socket = context(&[("s", b"|sock")]);
    let file = context(&[("s", b"")]);
    let opts = Opts::default();
    assert_eq!(last(df, &socket.path, &opts), last(df, &file.path, &opts));
}

#[test]
fn a_direct_copy_leaves_special_files_out_of_its_key() {
    let df = "FROM base\nCOPY . /\n";
    let special = context(&[("a.txt", b"a"), ("pipe", b"|fifo"), ("s", b"|sock")]);
    let plain = context(&[("a.txt", b"a")]);
    let direct = Opts {
        isolation: Isolation::Direct,
        ..Opts::default()
    };
    assert_eq!(
        last(df, &special.path, &direct),
        last(df, &plain.path, &direct)
    );
    assert_ne!(
        last(df, &special.path, &Opts::default()),
        last(df, &plain.path, &Opts::default())
    );
}

#[test]
fn hardlinks_in_a_copied_directory_are_keyed_as_links() {
    let df = "FROM base\nCOPY d/ /d/\n";
    let linked = context(&[("d/a", b"same")]);
    std::fs::hard_link(linked.file("d/a"), linked.file("d/b")).unwrap();
    let copied = context(&[("d/a", b"same"), ("d/b", b"same")]);
    let opts = Opts::default();
    assert_ne!(last(df, &linked.path, &opts), last(df, &copied.path, &opts));
    // A file named on its own is copied alone, never as a link.
    let single = "FROM base\nCOPY d/b /b\n";
    assert_eq!(
        last(single, &linked.path, &opts),
        last(single, &copied.path, &opts)
    );
}

#[test]
fn a_source_is_keyed_by_the_name_it_is_copied_under() {
    // `COPY l* /d/` writes /d/la in one and /d/lb in the other.
    let df = "FROM base\nCOPY l* /d/\n";
    let a = context(&[("x", b"x"), ("la", b"->x")]);
    let b = context(&[("x", b"x"), ("lb", b"->x")]);
    let opts = Opts::default();
    assert_ne!(last(df, &a.path, &opts), last(df, &b.path, &opts));
}

#[test]
fn an_archive_add_is_keyed_by_the_archive() {
    let df = "FROM base\nADD a.tar /x/\n";
    let a = context(&[("a.tar", &tar(b"one"))]);
    let b = context(&[("a.tar", &tar(b"two"))]);
    let opts = Opts::default();
    let ka = last(df, &a.path, &opts);
    assert_ne!(ka, last(df, &b.path, &opts));
    let copy = "FROM base\nCOPY a.tar /x/\n";
    assert_ne!(ka, last(copy, &a.path, &opts), "the instruction says ADD");
}

/// A one-member tar.
fn tar(content: &[u8]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_ustar();
    h.set_size(content.len() as u64);
    h.set_mode(0o644);
    h.set_cksum();
    b.append_data(&mut h, "member", content).unwrap();
    b.into_inner().unwrap()
}

/// A `--chown` resolves in the stage's root, which the plan does not have:
/// no key until #168 says what stands for the ids; the same steps without
/// it have keys.
#[test]
fn a_chown_step_has_no_key_until_its_owner_can_be_keyed() {
    let ctx = context(&[("run.sh", b"#!/bin/sh\n")]);
    let steps = |df: &str| {
        keys(df, &ctx.path, &Opts::default())
            .pop()
            .flatten()
            .unwrap()
            .steps()
            .to_vec()
    };
    for (owner, df) in [
        (
            "--chown=1:1 ",
            "FROM base\nCOPY {} run.sh /bin/\nRUN true\n",
        ),
        ("--chown=app ", "FROM base\nADD {} run.sh /bin/\n"),
        (
            "--chown=0:0 ",
            "FROM base AS one\nFROM base\nCOPY --from=one {} /etc /etc\n",
        ),
    ] {
        let with = steps(&df.replace("{} ", owner));
        assert!(with.iter().all(Option::is_none), "{df} {owner}");
        let without = steps(&df.replace("{} ", ""));
        assert!(without.iter().all(Option::is_some), "{df}");
    }
}

#[test]
fn a_stage_built_on_a_stage_chains_from_its_final_key() {
    let ctx = context(&[("a.txt", b"a")]);
    let df = "FROM base AS one\nCOPY a.txt /\nFROM one\nRUN true\n";
    let stages = keys(df, &ctx.path, &Opts::default());
    let (one, two) = (stages[0].as_ref().unwrap(), stages[1].as_ref().unwrap());
    assert_eq!((one.base_stage(), two.base_stage()), (None, Some(0)));
    assert_eq!(Some(two.root()), one.final_key());
    assert!(two.steps()[0].is_some());
    let df = "FROM base AS one\nCOPY --chown=1 a.txt /\nFROM one\nRUN true\n";
    let stages = keys(df, &ctx.path, &Opts::default());
    assert!(stages[1].is_none(), "built on a stage with no final key");
}

/// The keys the plan derives, pinned: rnk-cache's goldens pin how inputs
/// are hashed, these which inputs a Dockerfile and a context give.
#[test]
fn the_inputs_a_build_gives_are_pinned() {
    let ctx = context(&[("a.txt", b"hello\n")]);
    let df = "FROM base\nARG V=1\nENV MODE=release\nUSER app\nWORKDIR /srv/app\n\
              COPY --chmod=755 a.txt ./\n\
              RUN --mount=type=cache,target=/c --mount=type=secret,id=t,env=T <<EOF\n\
              echo $V\nEOF\n";
    let hex = |k: CacheKey| Digest::from_sha256_bytes(k.hash()).encoded().to_owned();
    assert_eq!(
        hex(last(df, &ctx.path, &Opts::default())),
        "6aa888aa2ba053e7df528f17543cba13fdc7142e162e545bf6cc4653fe18e9b4",
        "the plan's inputs changed: bump KEY_VERSION (now {KEY_VERSION}) and \
         update this golden with rnk-cache's"
    );
}

#[test]
fn a_step_the_build_refuses_has_no_key_nor_has_what_follows() {
    let ctx = context(&[("a.txt", b"a")]);
    let df = "FROM base AS one\nCOPY a.txt /\nADD https://example.com/a /a\nRUN true\n\
              FROM scratch\nCOPY --from=one /a /a\n";
    let keys = keys(df, &ctx.path, &Opts::default());
    let one = keys[0].as_ref().unwrap();
    assert!(one.steps()[0].is_some());
    assert!(one.steps()[1].is_none() && one.steps()[2].is_none());
    assert_eq!(one.final_key(), None);
    let two = keys[1].as_ref().unwrap();
    assert!(two.steps()[0].is_none(), "a stage reading it gets none");
}
