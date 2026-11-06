//! What changes a key and what never does.

use std::collections::HashMap;
use std::io::{self, Read};

use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;
use rnk_cache::{
    CacheKey, ContentHasher, CopyFrom, Encoding, EntryId, File, FileKind, KeyInputs, Mount,
    Network, Sharing, TimePolicy,
};
use rnk_oci::{Compression, Platform};

use crate::support::{EPOCH, Spec, content, digest, encoding, file, full, linux_amd64, root};

fn name() -> impl Strategy<Value = String> {
    "[A-Za-z_]{0,4}"
}

fn text() -> impl Strategy<Value = String> {
    "\\PC{0,6}"
}

fn bytes() -> impl Strategy<Value = Vec<u8>> {
    vec(any::<u8>(), 0..6)
}

fn file_kind() -> impl Strategy<Value = FileKind> {
    prop_oneof![
        vec(any::<u8>(), 0..64).prop_map(|b| FileKind::Regular(content(&b))),
        Just(FileKind::Directory),
        bytes().prop_map(FileKind::Symlink),
        bytes().prop_map(FileKind::Hardlink),
        Just(FileKind::Fifo),
        (any::<u32>(), any::<u32>())
            .prop_map(|(major, minor)| FileKind::CharDevice { major, minor }),
        (any::<u32>(), any::<u32>())
            .prop_map(|(major, minor)| FileKind::BlockDevice { major, minor }),
    ]
}

/// Near the epochs [`spec`] picks, so files fall on both sides of one.
fn mtime() -> impl Strategy<Value = i64> {
    -8..8_i64
}

fn a_file() -> impl Strategy<Value = File> {
    (
        bytes(),
        file_kind(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        option::of(bytes()),
        mtime(),
    )
        .prop_map(|(path, kind, mode, uid, gid, capability, mtime)| File {
            path,
            kind,
            mode,
            uid,
            gid,
            capability,
            mtime,
        })
}

fn mount() -> impl Strategy<Value = Mount> {
    let sharing = prop_oneof![
        Just(Sharing::Shared),
        Just(Sharing::Private),
        Just(Sharing::Locked)
    ];
    prop_oneof![
        (text(), text(), any::<u32>(), any::<u32>(), any::<u32>()).prop_map(
            |(id, target, mode, uid, gid)| Mount::Secret {
                id,
                target,
                mode,
                uid,
                gid
            }
        ),
        (text(), sharing, any::<bool>()).prop_map(|(id, sharing, dropped)| Mount::Cache {
            id,
            sharing,
            dropped
        }),
    ]
}

fn spec() -> impl Strategy<Value = Spec> {
    let network = prop_oneof![
        Just(Network::Default),
        Just(Network::None),
        Just(Network::Host)
    ];
    let from = prop_oneof![
        any::<u8>().prop_map(|seed| CopyFrom::Image(digest(seed))),
        any::<u8>()
            .prop_map(|seed| CopyFrom::Stage(CacheKey::root(Some(&digest(seed)), &linux_amd64()))),
    ];
    (
        option::of(-4..4_i64),
        text(),
        vec((name(), option::of(text())), 0..4),
        vec((name(), text()), 0..4),
        text(),
        text(),
        vec(text(), 0..3),
        option::of(network),
        vec(mount(), 0..3),
        option::of(from),
        vec(a_file(), 0..6),
    )
        .prop_map(
            |(
                epoch,
                instruction,
                args,
                env,
                user,
                workdir,
                shell,
                network,
                mounts,
                from,
                files,
            )| Spec {
                epoch,
                instruction,
                args,
                env,
                user,
                workdir,
                shell,
                network,
                mounts,
                from,
                files,
            },
        )
}

/// A named change to one field of a `T`.
type Change<T> = (&'static str, fn(&mut T));

/// Changes to one field of a step each: of the first entry of its kind, or
/// an added one, so each applies to [`full`] or to one of its twins.
fn mutations() -> Vec<Change<Spec>> {
    vec![
        ("instruction", |s| s.instruction.push('x')),
        ("arg added", |s| s.args.push(("NEW".to_owned(), None))),
        ("arg name", |s| {
            if let Some(a) = s.args.first_mut() {
                a.0.push('x');
            }
        }),
        ("arg value set or unset", |s| {
            if let Some(a) = s.args.first_mut() {
                a.1 = match a.1 {
                    None => Some(String::new()),
                    Some(_) => None,
                };
            }
        }),
        ("arg value", |s| {
            if let Some(v) = s.args.iter_mut().find_map(|a| a.1.as_mut()) {
                v.push('x');
            }
        }),
        ("env added", |s| {
            s.env.push(("NEW".to_owned(), String::new()))
        }),
        ("env name", |s| {
            if let Some(e) = s.env.first_mut() {
                e.0.push('x');
            }
        }),
        ("env value", |s| {
            if let Some(e) = s.env.first_mut() {
                e.1.push('x');
            }
        }),
        ("user", |s| s.user.push('x')),
        ("workdir", |s| s.workdir.push('x')),
        ("shell word added", |s| s.shell.push(String::new())),
        ("shell word", |s| {
            if let Some(w) = s.shell.first_mut() {
                w.push('x');
            }
        }),
        ("network", |s| {
            s.network = match s.network {
                None => Some(Network::Default),
                Some(Network::Default) => Some(Network::None),
                Some(Network::None) => Some(Network::Host),
                Some(Network::Host) => None,
            };
        }),
        ("mount added", |s| {
            s.mounts.push(Mount::Cache {
                id: String::new(),
                sharing: Sharing::Shared,
                dropped: false,
            });
        }),
        ("mounts reordered", |s| s.mounts.reverse()),
        ("secret id", |s| {
            if let Some(Mount::Secret { id, .. }) = secret(s) {
                id.push('x');
            }
        }),
        ("secret target", |s| {
            if let Some(Mount::Secret { target, .. }) = secret(s) {
                target.push('x');
            }
        }),
        ("secret mode", |s| {
            if let Some(Mount::Secret { mode, .. }) = secret(s) {
                *mode ^= 1;
            }
        }),
        ("secret uid", |s| {
            if let Some(Mount::Secret { uid, .. }) = secret(s) {
                *uid ^= 1;
            }
        }),
        ("secret gid", |s| {
            if let Some(Mount::Secret { gid, .. }) = secret(s) {
                *gid ^= 1;
            }
        }),
        ("cache id", |s| {
            if let Some(Mount::Cache { id, .. }) = cache(s) {
                id.push('x');
            }
        }),
        ("cache sharing", |s| {
            if let Some(Mount::Cache { sharing, .. }) = cache(s) {
                *sharing = match sharing {
                    Sharing::Shared => Sharing::Private,
                    Sharing::Private => Sharing::Locked,
                    Sharing::Locked => Sharing::Shared,
                };
            }
        }),
        ("cache dropped", |s| {
            if let Some(Mount::Cache { dropped, .. }) = cache(s) {
                *dropped = !*dropped;
            }
        }),
        ("copy source", |s| {
            s.from = match &s.from {
                None => Some(CopyFrom::Image(digest(0))),
                Some(CopyFrom::Image(_)) => Some(CopyFrom::Stage(root())),
                Some(CopyFrom::Stage(_)) => None,
            };
        }),
        ("source image", |s| {
            if let Some(CopyFrom::Image(d)) = &mut s.from {
                *d = if *d == digest(0) {
                    digest(1)
                } else {
                    digest(0)
                };
            }
        }),
        ("source stage", |s| {
            if let Some(CopyFrom::Stage(k)) = &mut s.from {
                *k = k.next("", KeyInputs::new(&encoding(None)));
            }
        }),
        ("file added", |s| {
            s.files.push(file("", FileKind::Directory))
        }),
        ("file removed", |s| {
            s.files.pop();
        }),
        ("file path", |s| {
            if let Some(f) = s.files.first_mut() {
                f.path.push(b'x');
            }
        }),
        ("--reproducible set or unset", |s| {
            if !s.files.is_empty() {
                s.epoch = match s.epoch {
                    None => Some(0),
                    Some(_) => None,
                };
            }
        }),
        ("file mtime under --reproducible", |s| {
            if let (Some(epoch), Some(f)) = (s.epoch, s.files.first_mut()) {
                let clamped = f.mtime.min(epoch);
                f.mtime = if clamped == epoch {
                    epoch - 1
                } else {
                    clamped + 1
                };
            }
        }),
        ("file mode", |s| {
            if let Some(f) = s.files.first_mut() {
                f.mode ^= 1;
            }
        }),
        ("file uid", |s| {
            if let Some(f) = s.files.first_mut() {
                f.uid ^= 1;
            }
        }),
        ("file gid", |s| {
            if let Some(f) = s.files.first_mut() {
                f.gid ^= 1;
            }
        }),
        ("file capability set or unset", |s| {
            if let Some(f) = s.files.first_mut() {
                f.capability = match f.capability {
                    None => Some(Vec::new()),
                    Some(_) => None,
                };
            }
        }),
        ("file capability", |s| {
            if let Some(c) = s.files.iter_mut().find_map(|f| f.capability.as_mut()) {
                c.push(0);
            }
        }),
        ("file type", |s| {
            if let Some(f) = s.files.first_mut() {
                f.kind = match f.kind {
                    FileKind::Directory => FileKind::Fifo,
                    FileKind::Regular(_)
                    | FileKind::Symlink(_)
                    | FileKind::Hardlink(_)
                    | FileKind::Fifo
                    | FileKind::CharDevice { .. }
                    | FileKind::BlockDevice { .. } => FileKind::Directory,
                };
            }
        }),
        ("file content", |s| {
            for f in &mut s.files {
                if let FileKind::Regular(c) = &mut f.kind {
                    *c = content(&[c.sha256()[0].wrapping_add(1)]);
                    break;
                }
            }
        }),
        ("symlink target", |s| {
            for f in &mut s.files {
                if let FileKind::Symlink(t) = &mut f.kind {
                    t.push(b'x');
                    break;
                }
            }
        }),
        ("hardlink target", |s| {
            for f in &mut s.files {
                if let FileKind::Hardlink(t) = &mut f.kind {
                    t.push(b'x');
                    break;
                }
            }
        }),
        ("symlink made a hardlink", |s| {
            for f in &mut s.files {
                if let FileKind::Symlink(t) = &f.kind {
                    f.kind = FileKind::Hardlink(t.clone());
                    break;
                }
            }
        }),
        ("device major", |s| {
            for f in &mut s.files {
                if let FileKind::CharDevice { major, .. } = &mut f.kind {
                    *major ^= 1;
                    break;
                }
            }
        }),
        ("device minor", |s| {
            for f in &mut s.files {
                if let FileKind::CharDevice { minor, .. } = &mut f.kind {
                    *minor ^= 1;
                    break;
                }
            }
        }),
        ("char device made a block device", |s| {
            for f in &mut s.files {
                if let FileKind::CharDevice { major, minor } = f.kind {
                    f.kind = FileKind::BlockDevice { major, minor };
                    break;
                }
            }
        }),
    ]
}

fn secret(s: &mut Spec) -> Option<&mut Mount> {
    s.mounts
        .iter_mut()
        .find(|m| matches!(m, Mount::Secret { .. }))
}

fn cache(s: &mut Spec) -> Option<&mut Mount> {
    s.mounts
        .iter_mut()
        .find(|m| matches!(m, Mount::Cache { .. }))
}

#[test]
fn every_field_of_a_step_changes_its_key() {
    let from_stage = Spec {
        from: Some(CopyFrom::Stage(root())),
        ..full()
    };
    let reproducible = Spec {
        epoch: Some(EPOCH),
        ..full()
    };
    let mut applied = HashMap::new();
    for base in [full(), from_stage, reproducible] {
        let mut seen = HashMap::from([(base.key(&root()), "nothing")]);
        for (what, mutate) in mutations() {
            let mut changed = base.clone();
            mutate(&mut changed);
            if changed == base {
                applied.entry(what).or_insert(false);
                continue;
            }
            applied.insert(what, true);
            if let Some(other) = seen.insert(changed.key(&root()), what) {
                panic!("changing {what} and changing {other} give one key");
            }
        }
        let scratch = CacheKey::root(None, &linux_amd64());
        assert_ne!(base.key(&root()), base.key(&scratch), "the previous key");
    }
    for (what, applied) in applied {
        assert!(applied, "the change of {what} applies to no fixture");
    }
}

#[test]
fn every_field_of_a_root_changes_its_key() {
    let full = Platform {
        architecture: "arm64".to_owned(),
        os: "linux".to_owned(),
        os_version: Some(String::new()),
        os_features: Some(Vec::new()),
        variant: Some(String::new()),
        features: Some(Vec::new()),
    };
    let mut platforms = vec![("base", full.clone())];
    let changes: [Change<Platform>; 10] = [
        ("architecture", |p| p.architecture.push('x')),
        ("os", |p| p.os.push('x')),
        ("os.version unset", |p| p.os_version = None),
        ("os.version", |p| p.os_version = Some("x".to_owned())),
        ("os.features unset", |p| p.os_features = None),
        ("os.features", |p| p.os_features = Some(vec![String::new()])),
        ("variant unset", |p| p.variant = None),
        ("variant", |p| p.variant = Some("x".to_owned())),
        ("features unset", |p| p.features = None),
        ("features", |p| p.features = Some(vec![String::new()])),
    ];
    for (what, change) in changes {
        let mut p = full.clone();
        change(&mut p);
        platforms.push((what, p));
    }
    let mut seen = HashMap::new();
    for base in [None, Some(digest(1)), Some(digest(2))] {
        for (what, platform) in &platforms {
            let key = CacheKey::root(base.as_ref(), platform);
            if let Some(other) = seen.insert(key, (base.clone(), *what)) {
                panic!("{what} with {base:?} and {other:?} give one key");
            }
        }
    }
}

#[test]
fn every_field_of_an_encoding_changes_the_tag() {
    let key = full().key(&root());
    let other = root().next("", KeyInputs::new(&encoding(None)));
    let mut seen = HashMap::new();
    for k in [key, other] {
        for (compression, levels) in [
            // The encoder ignores the level of a plain tar.
            (Compression::Uncompressed, &[0][..]),
            (Compression::Gzip, &[-1, 0, 1, 6, 9]),
            (Compression::Zstd, &[-1, 0, 1, 3, 19]),
        ] {
            for &level in levels {
                for times in [
                    TimePolicy::Kept,
                    TimePolicy::Clamped { epoch: 0 },
                    TimePolicy::Clamped { epoch: 1 },
                ] {
                    let encoding = Encoding {
                        compression,
                        level,
                        times,
                    };
                    let tag = EntryId::new(k, encoding).tag();
                    if let Some(other) = seen.insert(tag, (k, encoding)) {
                        panic!("{k:?} {encoding:?} and {other:?} give one tag");
                    }
                }
            }
        }
    }
}

#[test]
fn debug_names_arguments_and_variables_without_their_values() {
    let mut inputs = KeyInputs::new(&encoding(None));
    inputs
        .arg("ARG_NAME", Some("arg-value"))
        .env("ENV_NAME", "env-value");
    let shown = format!("{inputs:?}");
    assert!(
        shown.contains("ARG_NAME") && shown.contains("ENV_NAME"),
        "{shown}"
    );
    assert!(
        !shown.contains("arg-value") && !shown.contains("env-value"),
        "{shown}"
    );
}

#[test]
fn an_old_mtime_counts_only_under_reproducible() {
    let with = |mtime| Spec {
        files: vec![File {
            mtime,
            ..file("a", FileKind::Directory)
        }],
        ..Spec::default()
    };
    let (older, old) = (with(EPOCH - 2), with(EPOCH - 1));
    let (new, newer) = (with(EPOCH), with(EPOCH + 1));
    let key = |spec: &Spec, epoch| {
        Spec {
            epoch,
            ..spec.clone()
        }
        .key(&root())
    };
    assert_ne!(key(&older, Some(EPOCH)), key(&old, Some(EPOCH)));
    assert_ne!(key(&old, Some(EPOCH)), key(&new, Some(EPOCH)));
    // A later mtime is clamped to the epoch, as the header carries it.
    assert_eq!(key(&new, Some(EPOCH)), key(&newer, Some(EPOCH)));
    assert_eq!(key(&older, None), key(&old, None));
    assert_eq!(key(&old, None), key(&newer, None));
}

#[test]
fn a_key_ignores_the_compression_and_its_level() {
    let spec = full();
    for times in [TimePolicy::Kept, TimePolicy::Clamped { epoch: EPOCH }] {
        let key = |compression, level| {
            let encoding = Encoding {
                compression,
                level,
                times,
            };
            root().next(&spec.instruction, spec.encoded_as(&encoding))
        };
        let gzip = key(Compression::Gzip, 6);
        assert_eq!(gzip, key(Compression::Gzip, 9), "{times:?}");
        assert_eq!(gzip, key(Compression::Zstd, 3), "{times:?}");
        assert_eq!(gzip, key(Compression::Uncompressed, 0), "{times:?}");
    }
}

#[test]
fn an_uncompressed_tag_ignores_the_level() {
    let tag = |level| {
        let encoding = Encoding {
            compression: Compression::Uncompressed,
            level,
            times: TimePolicy::Kept,
        };
        EntryId::new(root(), encoding).tag()
    };
    assert_eq!(tag(0), tag(6));
}

#[test]
fn a_tag_is_an_oci_tag_naming_the_version() {
    let tag = EntryId::new(root(), encoding(None)).tag();
    let hex = tag.strip_prefix("rnk-v1-").expect("the rnk-v1- prefix");
    assert_eq!(hex.len(), 64, "{tag}");
    assert!(
        hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{tag}"
    );
    // The distribution spec's `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
    assert!(tag.len() <= 128, "{tag}");
}

/// A reader that gives at most `step` bytes per read.
struct Trickle<'a> {
    data: &'a [u8],
    step: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.step.min(buf.len()).min(self.data.len());
        let (head, rest) = self.data.split_at(n);
        buf[..n].copy_from_slice(head);
        self.data = rest;
        Ok(n)
    }
}

proptest! {
    #[test]
    fn the_order_of_args_env_and_files_never_changes_the_key(
        (spec, args, env, files) in spec().prop_flat_map(|s| {
            let args = Just(s.args.clone()).prop_shuffle();
            let env = Just(s.env.clone()).prop_shuffle();
            let files = Just(s.files.clone()).prop_shuffle();
            (Just(s), args, env, files)
        })
    ) {
        let shuffled = Spec { args, env, files, ..spec.clone() };
        prop_assert_eq!(spec.key(&root()), shuffled.key(&root()));
    }

    #[test]
    fn chunking_never_changes_the_content(
        data in vec(any::<u8>(), 0..4096),
        cuts in vec(1..512_usize, 1..16),
    ) {
        let whole = content(&data);
        let mut chunked = ContentHasher::new();
        let mut rest = &data[..];
        for cut in cuts.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (head, tail) = rest.split_at((*cut).min(rest.len()));
            chunked.update(head);
            chunked.update(&[]);
            rest = tail;
        }
        prop_assert_eq!(chunked.finish(), whole);
        let mut copied = ContentHasher::new();
        let n = io::copy(&mut Trickle { data: &data, step: cuts[0] }, &mut copied).unwrap();
        prop_assert_eq!(n, u64::try_from(data.len()).unwrap());
        prop_assert_eq!(copied.finish(), whole);
    }

    #[test]
    fn mtimes_the_header_does_not_carry_never_change_the_key(
        (spec, mtimes) in spec().prop_flat_map(|s| {
            let n = s.files.len();
            (Just(s), vec(mtime(), n))
        })
    ) {
        // Each file's mtime, moved where its header still carries the same.
        let moved = Spec {
            files: spec
                .files
                .iter()
                .zip(mtimes)
                .map(|(f, m)| File {
                    mtime: match spec.epoch {
                        None => m,
                        Some(epoch) if f.mtime >= epoch => epoch + m.abs(),
                        Some(_) => f.mtime,
                    },
                    ..f.clone()
                })
                .collect(),
            ..spec.clone()
        };
        prop_assert_eq!(spec.key(&root()), moved.key(&root()));
    }

    #[test]
    fn changing_any_one_field_changes_the_key(spec in spec()) {
        let key = spec.key(&root());
        for (what, mutate) in mutations() {
            let mut changed = spec.clone();
            mutate(&mut changed);
            if changed != spec {
                prop_assert_ne!(changed.key(&root()), key, "{}", what);
            }
        }
    }

    #[test]
    fn field_boundaries_cannot_shift(a in text(), b in "\\PC{1,3}", c in text()) {
        // ("ab", "c") against ("a", "bc"), for each pair of fields that meet.
        let (ab, bc) = (format!("{a}{b}"), format!("{b}{c}"));
        let pair = |left: &str, right: &str| -> [Spec; 7] {
            let (l, r) = (left.to_owned(), right.to_owned());
            [
                Spec { instruction: l.clone(), user: r.clone(), ..Spec::default() },
                Spec { args: vec![(l.clone(), Some(r.clone()))], ..Spec::default() },
                Spec { env: vec![(l.clone(), r.clone())], ..Spec::default() },
                Spec { user: l.clone(), workdir: r.clone(), ..Spec::default() },
                Spec { shell: vec![l.clone(), r.clone()], ..Spec::default() },
                Spec {
                    mounts: vec![Mount::Secret { id: l.clone(), target: r.clone(), mode: 0, uid: 0, gid: 0 }],
                    ..Spec::default()
                },
                Spec {
                    files: vec![file(&l, FileKind::Symlink(r.into_bytes()))],
                    ..Spec::default()
                },
            ]
        };
        for (x, y) in pair(&ab, &c).iter().zip(pair(&a, &bc).iter()) {
            prop_assert_ne!(x.key(&root()), y.key(&root()), "{:?} {:?}", x, y);
        }
        prop_assert_ne!(
            CacheKey::root(None, &Platform::new(c.clone(), ab.clone())),
            CacheKey::root(None, &Platform::new(bc.clone(), a.clone())),
        );
    }
}
