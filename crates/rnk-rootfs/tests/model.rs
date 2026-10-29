//! Property test: `RootPath::open_at` against a userspace model of
//! `RESOLVE_IN_ROOT`, on random trees of directories, files and symlinks
//! (absolute, relative, climbing, looping, pointing at the host).
//!
//! Wherever the model resolves a path, the kernel must reach the same entry;
//! wherever the model fails, the kernel must fail; and no path may ever
//! reach the file outside the root. Fixed seeds keep failures reproducible.
//!
//! T0: everything lives below fresh temp dirs, removed on drop.

use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use rnk_rootfs::{OpenHow, RootPath};

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

enum Node {
    Dir(Vec<(String, usize)>),
    File(String),
    Symlink(String),
}

/// The in-memory tree; node 0 is the root.
struct Model {
    nodes: Vec<Node>,
}

impl Model {
    fn child(&self, dir: usize, name: &str) -> Option<usize> {
        match &self.nodes[dir] {
            Node::Dir(entries) => entries.iter().find(|(n, _)| n == name).map(|(_, i)| *i),
            Node::File(_) | Node::Symlink(_) => None,
        }
    }

    /// `RESOLVE_IN_ROOT` semantics, following the final symlink: `None` when
    /// the kernel must fail.
    fn resolve(&self, path: &str) -> Option<usize> {
        let mut queue: std::collections::VecDeque<String> =
            path.split('/').map(str::to_owned).collect();
        let mut stack = vec![0usize];
        let mut hops = 0;
        while let Some(c) = queue.pop_front() {
            if c.is_empty() {
                continue;
            }
            // Any component after a non-directory, `.` and `..` included,
            // is `ENOTDIR`: the kernel does not fold `file/..` away.
            let cur = *stack.last()?;
            if !matches!(self.nodes[cur], Node::Dir(_)) {
                return None;
            }
            match c.as_str() {
                "." => {}
                ".." => {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                }
                name => {
                    let next = self.child(cur, name)?;
                    match &self.nodes[next] {
                        Node::Symlink(target) => {
                            hops += 1;
                            if hops > 40 {
                                return None;
                            }
                            if target.starts_with('/') {
                                stack.truncate(1);
                            }
                            for t in target.split('/').rev() {
                                queue.push_front(t.to_owned());
                            }
                        }
                        Node::Dir(_) | Node::File(_) => stack.push(next),
                    }
                }
            }
        }
        // A path ending in a component after a file fails above; a path
        // naming a file through a trailing `..` or `.` does not exist here.
        stack.last().copied()
    }
}

const NAMES: &[&str] = &["a", "b", "c"];

/// Builds a random tree both in memory and on disk below `host`.
fn build(rng: &mut Rng, host: &Path, outside: &Path) -> Model {
    let mut model = Model {
        nodes: vec![Node::Dir(Vec::new())],
    };
    let mut dirs = vec![(0usize, host.to_owned(), 0usize)];
    let mut file_id = 0;
    while let Some((idx, dir, depth)) = dirs.pop() {
        for name in NAMES {
            let p = dir.join(name);
            let kind = if depth >= 3 {
                1 + rng.below(3)
            } else {
                rng.below(4)
            };
            let node = match kind {
                0 => {
                    fs::create_dir(&p).unwrap();
                    Node::Dir(Vec::new())
                }
                1 => {
                    file_id += 1;
                    let id = format!("file{file_id}");
                    fs::write(&p, &id).unwrap();
                    Node::File(id)
                }
                2 => continue,
                _ => {
                    let target = random_target(rng, outside);
                    symlink(&target, &p).unwrap();
                    Node::Symlink(target)
                }
            };
            let is_dir = matches!(node, Node::Dir(_));
            model.nodes.push(node);
            let new = model.nodes.len() - 1;
            if let Node::Dir(entries) = &mut model.nodes[idx] {
                entries.push(((*name).to_owned(), new));
            }
            if is_dir {
                dirs.push((new, p, depth + 1));
            }
        }
    }
    model
}

fn random_target(rng: &mut Rng, outside: &Path) -> String {
    match rng.below(8) {
        0 => outside.join("secret").to_str().unwrap().to_owned(),
        1 => outside.to_str().unwrap().to_owned(),
        2 => "/proc/self/root/etc".to_owned(),
        _ => random_path(rng, true),
    }
}

fn random_path(rng: &mut Rng, may_be_absolute: bool) -> String {
    let len = 1 + rng.below(5);
    let mut parts: Vec<&str> = Vec::new();
    for _ in 0..len {
        parts.push(rng.pick(&["a", "b", "c", "..", ".", "a", "b", "c"]));
    }
    let p = parts.join("/");
    if may_be_absolute && rng.below(3) == 0 {
        format!("/{p}")
    } else {
        p
    }
}

#[test]
fn open_at_matches_the_model() {
    let mut checked = 0;
    for seed in 1..=150u64 {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let host = base.join("root");
        let outside = base.join("outside");
        fs::create_dir(&host).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), "OUTSIDE").unwrap();
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let model = build(&mut rng, &host, &outside);
        let root = RootPath::open(&host).unwrap();
        for _ in 0..60 {
            let p = random_path(&mut rng, true);
            let got = root.open_at(&p, &OpenHow::read());
            let want = model.resolve(&p);
            match (want.map(|i| &model.nodes[i]), got) {
                (None, Ok(_)) => panic!("seed {seed}: {p:?} must fail"),
                (None, Err(_)) => {}
                (Some(_), Err(e)) => panic!("seed {seed}: {p:?} failed: {e}"),
                (Some(Node::File(id)), Ok(fd)) => {
                    let mut s = String::new();
                    fs::File::from(fd).read_to_string(&mut s).unwrap();
                    assert_eq!(&s, id, "seed {seed}: {p:?}");
                    checked += 1;
                }
                (Some(Node::Dir(_)), Ok(fd)) => {
                    assert!(fs::File::from(fd).metadata().unwrap().is_dir());
                    checked += 1;
                }
                (Some(Node::Symlink(_)), Ok(_)) => {
                    panic!("seed {seed}: the model never stops on a symlink")
                }
            }
        }
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"OUTSIDE");
    }
    // The generator must actually reach entries, not only fail.
    assert!(checked > 1000, "only {checked} paths resolved");
}

/// A relative path is resolved like the same path made absolute.
#[test]
fn relative_and_absolute_paths_agree() {
    let tmp = tempfile::tempdir().unwrap();
    let host = fs::canonicalize(tmp.path()).unwrap().join("root");
    fs::create_dir_all(host.join("a/b")).unwrap();
    fs::write(host.join("a/b/f"), "x").unwrap();
    let root = RootPath::open(&host).unwrap();
    for p in ["a/b/f", "./a/b/f", "a/./b/f", "a/b/../b/f", "../a/b/f"] {
        let abs = PathBuf::from("/").join(p);
        assert!(root.open_at(p, &OpenHow::read()).is_ok(), "{p}");
        assert!(root.open_at(&abs, &OpenHow::read()).is_ok(), "{abs:?}");
    }
}
