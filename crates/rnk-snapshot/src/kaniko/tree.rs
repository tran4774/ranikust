//! The result of one walk, kept for the next one as a path arena: kaniko's
//! `LayeredMap` without a string per path. Every name lives once in one
//! byte buffer; a node points at its name, its parent and the contiguous,
//! name-sorted run of its children, so a directory's previous entry for a
//! name is a binary search away.

use std::collections::HashMap;
use std::path::PathBuf;

use super::walk::Record;

/// The index of the root node.
pub(crate) const ROOT: u32 = 0;

/// One entry of a walked tree.
#[derive(Clone, Debug)]
pub(crate) struct Node {
    name: (u32, u32),
    parent: u32,
    children: (u32, u32),
    pub(crate) rec: Record,
}

/// A walked tree.
#[derive(Debug)]
pub(crate) struct Tree {
    names: Vec<u8>,
    nodes: Vec<Node>,
}

/// One directory as a walk task saw it: its path relative to the root
/// (empty for the root) and its entries, their names concatenated.
#[derive(Debug, Default)]
pub(crate) struct DirOut {
    pub(crate) path: Vec<u8>,
    pub(crate) names: Vec<u8>,
    pub(crate) entries: Vec<((u32, u32), Record)>,
}

impl DirOut {
    pub(crate) fn push(&mut self, name: &[u8], rec: Record) {
        let start = len_u32(self.names.len());
        self.names.extend_from_slice(name);
        self.entries.push(((start, len_u32(name.len())), rec));
    }
}

/// Lengths and offsets in a tree fit in `u32`: a walk of more than 4 GiB of
/// names or 4 billion entries has run out of memory long before.
fn len_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Tree {
    /// Builds the tree of a walk from its root's record and every
    /// directory's output, in breadth-first order so that each directory's
    /// children are contiguous and sorted by name. The order of `dirs`
    /// does not matter: the result depends only on what was walked.
    pub(crate) fn build(root: Record, dirs: Vec<DirOut>) -> Self {
        let by_path: HashMap<&[u8], &DirOut> = dirs.iter().map(|d| (&d.path[..], d)).collect();
        let total: usize = dirs.iter().map(|d| d.entries.len()).sum();
        let mut tree = Tree {
            names: Vec::with_capacity(dirs.iter().map(|d| d.names.len()).sum()),
            nodes: Vec::with_capacity(total + 1),
        };
        tree.nodes.push(Node {
            name: (0, 0),
            parent: ROOT,
            children: (0, 0),
            rec: root,
        });
        let mut queue: std::collections::VecDeque<(u32, Vec<u8>)> =
            std::collections::VecDeque::from([(ROOT, Vec::new())]);
        while let Some((id, path)) = queue.pop_front() {
            let Some(dir) = by_path.get(&path[..]) else {
                continue;
            };
            let name = |e: &((u32, u32), Record)| {
                let (s, l) = e.0;
                &dir.names[s as usize..(s + l) as usize]
            };
            let mut order: Vec<usize> = (0..dir.entries.len()).collect();
            order.sort_unstable_by(|a, b| name(&dir.entries[*a]).cmp(name(&dir.entries[*b])));
            let first = len_u32(tree.nodes.len());
            for i in order {
                let e = &dir.entries[i];
                let n = name(e);
                let start = len_u32(tree.names.len());
                tree.names.extend_from_slice(n);
                let child = len_u32(tree.nodes.len());
                tree.nodes.push(Node {
                    name: (start, len_u32(n.len())),
                    parent: id,
                    children: (0, 0),
                    rec: e.1.clone(),
                });
                if e.1.is_dir() {
                    let mut p = path.clone();
                    if !p.is_empty() {
                        p.push(b'/');
                    }
                    p.extend_from_slice(n);
                    queue.push_back((child, p));
                }
            }
            let count = len_u32(tree.nodes.len()) - first;
            if let Some(node) = tree.nodes.get_mut(id as usize) {
                node.children = (first, count);
            }
        }
        tree
    }

    /// Number of nodes, the root included.
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn node(&self, id: u32) -> &Node {
        &self.nodes[id as usize]
    }

    pub(crate) fn name(&self, id: u32) -> &[u8] {
        let (s, l) = self.node(id).name;
        &self.names[s as usize..(s + l) as usize]
    }

    pub(crate) fn parent(&self, id: u32) -> u32 {
        self.node(id).parent
    }

    /// The child of directory `dir` named `name`.
    pub(crate) fn child(&self, dir: u32, name: &[u8]) -> Option<u32> {
        let (first, count) = self.node(dir).children;
        let mut lo = first;
        let mut hi = first + count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.name(mid).cmp(name) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The path of node `id`, relative to the root.
    pub(crate) fn path(&self, id: u32) -> PathBuf {
        use std::os::unix::ffi::OsStrExt as _;
        let mut parts = Vec::new();
        let mut n = id;
        while n != ROOT {
            parts.push(self.name(n));
            n = self.parent(n);
        }
        let mut p = PathBuf::new();
        for part in parts.iter().rev() {
            p.push(std::ffi::OsStr::from_bytes(part));
        }
        p
    }
}
