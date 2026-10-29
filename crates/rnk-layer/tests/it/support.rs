//! Helpers shared by the integration tests: an in-memory entry source, a
//! sample tree and a tar reader built on the `tar` crate.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read};
use std::num::NonZeroUsize;

use rnk_layer::{
    Compression, EncodeOptions, Entry, EntryKind, EntryPath, EntrySource, Error, LayerDigests,
    LayerWriter,
};
use sha2::{Digest, Sha256};

pub(crate) fn path(s: &str) -> EntryPath {
    EntryPath::new(s).unwrap()
}

/// An entry with typical metadata for its kind.
pub(crate) fn entry(p: &str, kind: EntryKind) -> Entry {
    let mode = match kind {
        EntryKind::Dir | EntryKind::OpaqueDir => 0o40_755,
        EntryKind::File { .. } => 0o100_644,
        EntryKind::Symlink(_) => 0o120_777,
        EntryKind::Char(..) => 0o20_666,
        EntryKind::Block(..) => 0o60_660,
        EntryKind::Fifo => 0o10_644,
        EntryKind::Whiteout => 0o20_000,
    };
    Entry {
        path: path(p),
        kind,
        mode,
        uid: 0,
        gid: 0,
        mtime: 1_700_000_000,
        xattrs: BTreeMap::new(),
        inode: None,
    }
}

/// Entries and file contents kept in memory.
#[derive(Clone, Debug, Default)]
pub(crate) struct MemSource {
    pub(crate) files: HashMap<EntryPath, Vec<u8>>,
    /// Directories the parent lookup knows.
    pub(crate) parents: HashMap<EntryPath, Entry>,
    /// Every parent lookup, in order.
    pub(crate) lookups: Vec<EntryPath>,
}

impl EntrySource for MemSource {
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>> {
        match self.files.get(&entry.path) {
            Some(data) => Ok(Box::new(&data[..])),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no content")),
        }
    }

    fn parent(&mut self, path: &EntryPath) -> io::Result<Option<Entry>> {
        self.lookups.push(path.clone());
        Ok(self.parents.get(path).cloned())
    }
}

/// A list of entries to feed and the source behind them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Tree {
    pub(crate) entries: Vec<Entry>,
    pub(crate) source: MemSource,
}

impl Tree {
    pub(crate) fn add(&mut self, entry: Entry) -> &mut Entry {
        self.entries.push(entry);
        self.entries.last_mut().unwrap()
    }

    pub(crate) fn file(&mut self, p: &str, content: &[u8]) -> &mut Entry {
        self.source.files.insert(path(p), content.to_vec());
        let size = u64::try_from(content.len()).unwrap();
        self.add(entry(p, EntryKind::File { size }))
    }

    pub(crate) fn dir(&mut self, p: &str, mode: u32) -> &mut Entry {
        let e = self.add(entry(p, EntryKind::Dir));
        e.mode = 0o40_000 | mode;
        e
    }

    /// A directory only the parent lookup knows.
    pub(crate) fn known_parent(&mut self, p: &str, mode: u32) {
        let mut e = entry(p, EntryKind::Dir);
        e.mode = 0o40_000 | mode;
        self.source.parents.insert(e.path.clone(), e);
    }

    pub(crate) fn encode(&mut self, options: EncodeOptions) -> Result<Vec<u8>, Error> {
        Ok(self.encode_with_digests(options)?.0)
    }

    pub(crate) fn encode_with_digests(
        &mut self,
        options: EncodeOptions,
    ) -> Result<(Vec<u8>, LayerDigests), Error> {
        let mut writer = LayerWriter::new(options)?;
        for e in &self.entries {
            writer.add(e.clone())?;
        }
        let mut out = Vec::new();
        let digests = writer.finish(&mut self.source, &mut out)?;
        Ok((out, digests))
    }
}

pub(crate) fn options(compression: Compression, threads: usize) -> EncodeOptions {
    EncodeOptions {
        compression,
        level: 6,
        threads: NonZeroUsize::new(threads).unwrap(),
        mtime: None,
    }
}

pub(crate) fn plain() -> EncodeOptions {
    options(Compression::None, 1)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A 124-byte path component: too long for a ustar name even when split.
pub(crate) fn long_component() -> String {
    "x".repeat(124)
}

/// A small tree with every kind of entry and every header feature.
pub(crate) fn sample_tree() -> Tree {
    let mut t = Tree::default();
    t.dir("etc", 0o755);
    t.file("etc/hostname", b"ranikust\n");
    t.add(entry(
        "etc/localtime",
        EntryKind::Symlink(b"/usr/share/zoneinfo/UTC".to_vec()),
    ));
    t.dir("tmp", 0o1777);
    // `usr` and `usr/bin` are known only to the parent lookup.
    t.known_parent("usr", 0o755);
    t.known_parent("usr/bin", 0o755);
    let ping = t.file("usr/bin/ping", b"\x7fELF ping");
    ping.mode = 0o104_755;
    ping.inode = Some((1, 42));
    ping.xattrs.insert(
        b"security.capability".to_vec(),
        vec![
            1, 0, 0, 2, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ],
    );
    let ping6 = t.file("usr/bin/ping6", b"\x7fELF ping");
    ping6.mode = 0o104_755;
    ping6.inode = Some((1, 42));
    t.known_parent("root", 0o700);
    t.add(entry("root/.cache", EntryKind::Whiteout));
    t.known_parent("opt", 0o755);
    t.add(entry("opt/app", EntryKind::OpaqueDir));
    t.file("opt/app/new", b"new\n");
    t.dir("dev", 0o755);
    t.add(entry("dev/null", EntryKind::Char(1, 3)));
    t.add(entry("dev/sda", EntryKind::Block(8, 0)));
    t.dir("run", 0o755);
    t.add(entry("run/fifo", EntryKind::Fifo));
    t.known_parent("deep", 0o755);
    t.known_parent(&format!("deep/{}", long_component()), 0o750);
    let target = format!("/deep/{}/file", long_component());
    t.file(&format!("deep/{}/file", long_component()), b"deep\n");
    t.add(entry("deep/link", EntryKind::Symlink(target.into_bytes())));
    t.known_parent("home", 0o755);
    t.known_parent("home/big", 0o755);
    let owned = t.file("home/big/owned", b"");
    owned.uid = 3_000_000;
    owned.gid = 3_000_001;
    owned.mtime = -1;
    t
}

/// One entry read back with the `tar` crate, PAX records applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadBack {
    pub(crate) name: Vec<u8>,
    pub(crate) typeflag: u8,
    pub(crate) mode: u32,
    pub(crate) uid: u64,
    pub(crate) gid: u64,
    pub(crate) mtime: i64,
    pub(crate) link: Option<Vec<u8>>,
    pub(crate) device: (u32, u32),
    pub(crate) xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub(crate) content: Vec<u8>,
}

impl ReadBack {
    pub(crate) fn name(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }
}

/// Reads a plain tar stream with the `tar` crate.
pub(crate) fn read_tar(bytes: &[u8]) -> Vec<ReadBack> {
    let mut archive = tar::Archive::new(bytes);
    let mut out = Vec::new();
    for e in archive.entries().unwrap() {
        let mut e = e.unwrap();
        let header = e.header().clone();
        let mut uid = header.uid().unwrap();
        let mut gid = header.gid().unwrap();
        let mut mtime = i64::try_from(header.mtime().unwrap()).unwrap();
        let mut xattrs = BTreeMap::new();
        if let Some(pax) = e.pax_extensions().unwrap() {
            for ext in pax {
                let ext = ext.unwrap();
                let value = std::str::from_utf8(ext.value_bytes());
                match ext.key_bytes() {
                    b"uid" => uid = value.unwrap().parse().unwrap(),
                    b"gid" => gid = value.unwrap().parse().unwrap(),
                    b"mtime" => mtime = value.unwrap().parse().unwrap(),
                    key => {
                        if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
                            xattrs.insert(name.to_vec(), ext.value_bytes().to_vec());
                        }
                    }
                }
            }
        }
        let device = (
            header.device_major().unwrap().unwrap_or(0),
            header.device_minor().unwrap().unwrap_or(0),
        );
        let mut content = Vec::new();
        e.read_to_end(&mut content).unwrap();
        out.push(ReadBack {
            name: e.path_bytes().into_owned(),
            typeflag: header.entry_type().as_byte(),
            mode: header.mode().unwrap(),
            uid,
            gid,
            mtime,
            link: e.link_name_bytes().map(|l| l.into_owned()),
            device,
            xattrs,
            content,
        });
    }
    out
}
