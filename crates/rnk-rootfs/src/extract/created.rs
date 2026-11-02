//! What a layer created, for its opaque whiteouts ([`super::Whiteouts::Apply`]).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

/// The entries a layer created that are still there, and the directories
/// leading to them, as a tree of names.
///
/// Each name is stored once, however long the paths through it (a live
/// root's physical paths can be much longer than the layer's names), and
/// what the layer deletes again is forgotten with everything below it, so
/// the tree is never larger than what the layer left on disk. A node that
/// is there and not created is a directory that led to a created entry at
/// some point, and stays one while the directory stays.
#[derive(Debug, Default)]
pub(super) struct Created {
    root: Node,
}

#[derive(Debug, Default)]
struct Node {
    /// The layer created this entry; else it only leads to one.
    created: bool,
    children: BTreeMap<OsString, Node>,
}

/// What [`Created::lookup`] says of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mark {
    /// The layer created it.
    Created,
    /// It leads to an entry the layer created.
    Ancestor,
    /// Neither.
    None,
}

impl Created {
    /// Records `path` (cleaned, relative, not the root) as created.
    pub(super) fn insert(&mut self, path: &Path) {
        if path.as_os_str().is_empty() {
            return;
        }
        let mut node = &mut self.root;
        for name in path.iter() {
            if !node.children.contains_key(name) {
                node.children.insert(name.to_owned(), Node::default());
            }
            let Some(next) = node.children.get_mut(name) else {
                return;
            };
            node = next;
        }
        node.created = true;
    }

    /// Whether `path` (cleaned, relative) was created by the layer, leads to
    /// something it created, or neither.
    pub(super) fn lookup(&self, path: &Path) -> Mark {
        let mut node = &self.root;
        for name in path.iter() {
            match node.children.get(name) {
                Some(next) => node = next,
                None => return Mark::None,
            }
        }
        if node.created {
            Mark::Created
        } else {
            Mark::Ancestor
        }
    }

    /// Forgets `path` and everything below it: it was deleted. The
    /// directories above it stay as they are, as they stay on disk: one
    /// that led only to `path` still led to an entry of this layer, so an
    /// opaque marker keeps it, as overlay's merged view does.
    pub(super) fn remove(&mut self, path: &Path) {
        let Some((parent, name)) = path.parent().zip(path.file_name()) else {
            return;
        };
        if let Some(node) = self.find(parent)
            && let Some(gone) = node.children.remove(name)
        {
            drop_iteratively(gone);
        }
    }

    /// Forgets everything below `path`, which stays as it was: a deletion
    /// inside it that had to keep something. What is still there below it
    /// is what the deletion kept, never something this layer wrote.
    pub(super) fn clear_below(&mut self, path: &Path) {
        if let Some(node) = self.find(path) {
            for (_, n) in std::mem::take(&mut node.children) {
                drop_iteratively(n);
            }
        }
    }

    fn find(&mut self, path: &Path) -> Option<&mut Node> {
        let mut node = &mut self.root;
        for name in path.iter() {
            node = node.children.get_mut(name)?;
        }
        Some(node)
    }

    #[cfg(test)]
    fn nodes(&self) -> usize {
        let mut n = 0;
        let mut stack = vec![&self.root];
        while let Some(node) = stack.pop() {
            n += node.children.len();
            stack.extend(node.children.values());
        }
        n
    }
}

impl Drop for Created {
    fn drop(&mut self) {
        drop_iteratively(std::mem::take(&mut self.root));
    }
}

/// Drops `node` and everything below it without recursion: the tree is as
/// deep as the deepest path the layer created.
fn drop_iteratively(node: Node) {
    let mut stack = vec![node.children];
    while let Some(children) = stack.pop() {
        for (_, mut n) in children {
            stack.push(std::mem::take(&mut n.children));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(paths: &[&str]) -> Created {
        let mut c = Created::default();
        for p in paths {
            c.insert(Path::new(p));
        }
        c
    }

    #[test]
    fn created_entries_and_their_ancestors() {
        let c = created(&["a/b/c", "a/d"]);
        assert_eq!(c.lookup(Path::new("a/b/c")), Mark::Created);
        assert_eq!(c.lookup(Path::new("a/d")), Mark::Created);
        assert_eq!(c.lookup(Path::new("a")), Mark::Ancestor);
        assert_eq!(c.lookup(Path::new("a/b")), Mark::Ancestor);
        assert_eq!(c.lookup(Path::new("a/b/c/e")), Mark::None);
        assert_eq!(c.lookup(Path::new("b")), Mark::None);
        // A directory the layer created and wrote into is both.
        let c = created(&["a", "a/b"]);
        assert_eq!(c.lookup(Path::new("a")), Mark::Created);
    }

    #[test]
    fn a_removed_entry_leaves_the_directories_above_it() {
        let mut c = created(&["a/b/c/d", "a/x"]);
        c.remove(Path::new("a/b/c/d"));
        assert_eq!(c.lookup(Path::new("a/b/c/d")), Mark::None);
        assert_eq!(c.lookup(Path::new("a/b/c")), Mark::Ancestor);
        assert_eq!(c.lookup(Path::new("a")), Mark::Ancestor);
        assert_eq!(c.nodes(), 4);
        c.remove(Path::new("a/b"));
        assert_eq!(c.lookup(Path::new("a/b")), Mark::None);
        assert_eq!(c.nodes(), 2);
    }

    #[test]
    fn a_removed_directory_takes_everything_below_it() {
        let mut c = created(&["a", "a/b/c", "a/b/d", "e"]);
        c.remove(Path::new("a/b"));
        // `a` was created itself: it stays.
        assert_eq!(c.lookup(Path::new("a")), Mark::Created);
        assert_eq!(c.lookup(Path::new("a/b/c")), Mark::None);
        assert_eq!(c.nodes(), 2);
        c.remove(Path::new("a"));
        assert_eq!(c.nodes(), 1);
    }

    #[test]
    fn removing_what_was_not_recorded_changes_nothing() {
        let mut c = created(&["a/b"]);
        c.remove(Path::new("a/c"));
        c.remove(Path::new("x/y"));
        c.remove(Path::new(""));
        assert_eq!(c.lookup(Path::new("a/b")), Mark::Created);
        assert_eq!(c.nodes(), 2);
    }

    #[test]
    fn clearing_below_keeps_the_entry_as_it_was() {
        let mut c = created(&["k", "k/a", "k/b/c", "n/a"]);
        c.clear_below(Path::new("k"));
        assert_eq!(c.lookup(Path::new("k")), Mark::Created);
        assert_eq!(c.lookup(Path::new("k/a")), Mark::None);
        c.clear_below(Path::new("n"));
        assert_eq!(c.lookup(Path::new("n")), Mark::Ancestor);
        assert_eq!(c.lookup(Path::new("n/a")), Mark::None);
        assert_eq!(c.nodes(), 2);
    }

    #[test]
    fn a_deep_tree_is_dropped_without_recursion() {
        let deep: std::path::PathBuf = std::iter::repeat_n("x", 200_000).collect();
        let mut c = created(&[deep.to_str().unwrap()]);
        c.remove(Path::new("x/x"));
        assert_eq!(c.nodes(), 1);
        c.insert(&deep);
        drop(c);
    }
}
