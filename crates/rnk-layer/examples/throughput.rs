//! Rough throughput of the layer writer with gzip and zstd, and a check that
//! thread counts give the same bytes. Prints numbers; asserts only the
//! equality.
//!
//! ```sh
//! cargo run --release -p rnk-layer --example throughput -- [MiB] [threads...]
//! ```

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::time::Instant;

use rnk_layer::{
    Compression, EncodeOptions, Entry, EntryKind, EntryPath, EntrySource, LayerWriter,
};

/// Bytes per file.
const FILE_SIZE: usize = 1 << 20;

/// Every file is a slice of one buffer.
struct Files {
    data: Vec<u8>,
}

impl EntrySource for Files {
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>> {
        let name = String::from_utf8_lossy(entry.path.file_name()).into_owned();
        let index: usize = name.parse().map_err(io::Error::other)?;
        let start = index * FILE_SIZE;
        Ok(Box::new(&self.data[start..start + FILE_SIZE]))
    }

    fn parent(&mut self, _: &EntryPath) -> io::Result<Option<Entry>> {
        Ok(None)
    }
}

/// About as compressible as a real layer: text runs mixed with noise.
fn data(len: usize) -> Vec<u8> {
    let words: &[&[u8]] = &[
        b"layer ",
        b"ranikust ",
        b"lib",
        b".so ",
        b"\0\0\0\0",
        b"function ",
        b"return ",
    ];
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let [a, b, ..] = x.to_le_bytes();
        if a < 90 {
            out.extend_from_slice(&x.to_le_bytes());
        } else {
            out.extend_from_slice(words[usize::from(b) % words.len()]);
        }
    }
    out.truncate(len);
    out
}

fn entry(path: &str, kind: EntryKind) -> Entry {
    Entry {
        path: EntryPath::new(path).expect("valid path"),
        kind,
        mode: 0o644,
        uid: 0,
        gid: 0,
        mtime: 0,
        xattrs: BTreeMap::new(),
        inode: None,
    }
}

fn encode(files: &mut Files, count: usize, compression: Compression, threads: usize) -> Vec<u8> {
    let options = EncodeOptions {
        compression,
        level: 6,
        threads: NonZeroUsize::new(threads).expect("threads > 0"),
        mtime: Some(0),
    };
    let mut writer = LayerWriter::new(options).expect("valid options");
    writer
        .add(entry("data", EntryKind::Dir))
        .expect("valid entry");
    let size = u64::try_from(FILE_SIZE).expect("fits");
    for i in 0..count {
        writer
            .add(entry(&format!("data/{i}"), EntryKind::File { size }))
            .expect("valid entry");
    }
    let mut out = Vec::new();
    writer.finish(files, &mut out).expect("encode");
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mib: usize = args.next().map_or(256, |a| a.parse().expect("MiB"));
    let mut threads: Vec<usize> = args.map(|a| a.parse().expect("threads")).collect();
    if threads.is_empty() {
        let all = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        threads = vec![1, 2, 4, all];
        threads.dedup();
    }
    let mut files = Files {
        data: data(mib * FILE_SIZE),
    };
    let input = f64::from(u32::try_from(mib).expect("MiB fits u32")) * f64::from(1u32 << 20);

    let start = Instant::now();
    let tar = encode(&mut files, mib, Compression::None, 1);
    println!(
        "plain tar: {:7.0} MB/s ({} bytes)",
        input / start.elapsed().as_secs_f64() / 1e6,
        tar.len()
    );

    for compression in [Compression::Gzip, Compression::Zstd] {
        let mut reference: Option<Vec<u8>> = None;
        for &t in &threads {
            let start = Instant::now();
            let blob = encode(&mut files, mib, compression, t);
            let secs = start.elapsed().as_secs_f64();
            println!(
                "{compression} -6, {t:2} threads: {:7.0} MB/s ({} bytes)",
                input / secs / 1e6,
                blob.len()
            );
            match &reference {
                Some(r) => assert!(*r == blob, "{compression}: {t} threads gave other bytes"),
                None => reference = Some(blob),
            }
        }
    }
}
