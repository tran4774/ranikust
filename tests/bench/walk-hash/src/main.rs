//! Q-038 benchmark: walks a root and hashes its regular files with each
//! candidate, printing one JSON line per run.
//!
//! ```text
//! walk-hash-bench walk <fd|jwalk> <root> <threads> <none|xxh3|highway|sha256>
//! walk-hash-bench mem <root> <threads> <xxh3|highway|sha256>
//! ```
//!
//! `walk fd` is the design of `design/kaniko-mode.md#walk-hash-and-diff`:
//! one rayon task per directory, each directory opened relative to its
//! parent's descriptor with `openat2`, `getdents64` through `RawDir`,
//! `statx` per entry and the mount-id guard. `walk jwalk` reads by path,
//! as `jwalk` does, with the per-entry work in `process_read_dir` so it runs
//! on the walk's own threads. `mem` reads every regular file into memory
//! first and times the hash alone.

use std::ffi::CStr;
use std::fs::File;
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use highway::{HighwayHash, HighwayHasher, Key};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, ResolveFlags, StatxFlags};
use sha2::Digest;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const READ_BUF: usize = 128 * 1024;
const DIR_BUF: usize = 32 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Hash {
    None,
    Xxh3,
    Highway,
    Sha256,
}

impl Hash {
    fn parse(s: &str) -> Self {
        match s {
            "none" => Self::None,
            "xxh3" => Self::Xxh3,
            "highway" => Self::Highway,
            "sha256" => Self::Sha256,
            _ => panic!("unknown hash {s}"),
        }
    }

    /// Hashes everything `read` yields; returns a value folded into the
    /// checksum so the work cannot be optimised away.
    fn stream(self, mut read: impl FnMut(&mut [u8]) -> usize, buf: &mut [u8]) -> u64 {
        match self {
            Self::None => 0,
            Self::Xxh3 => {
                let mut h = xxhash_rust::xxh3::Xxh3::new();
                loop {
                    let n = read(buf);
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                h.digest128() as u64
            }
            Self::Highway => {
                let mut h = HighwayHasher::new(Key([1, 2, 3, 4]));
                loop {
                    let n = read(buf);
                    if n == 0 {
                        break;
                    }
                    h.append(&buf[..n]);
                }
                h.finalize128()[0]
            }
            Self::Sha256 => {
                let mut h = sha2::Sha256::new();
                loop {
                    let n = read(buf);
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                let d = h.finalize();
                u64::from_le_bytes(d[..8].try_into().unwrap())
            }
        }
    }

    /// The whole slice at once, as a file read in one go would be.
    fn bytes(self, data: &[u8]) -> u64 {
        match self {
            Self::None => 0,
            Self::Xxh3 => xxhash_rust::xxh3::xxh3_128(data) as u64,
            Self::Highway => HighwayHasher::new(Key([1, 2, 3, 4])).hash128(data)[0],
            Self::Sha256 => {
                let d = sha2::Sha256::digest(data);
                u64::from_le_bytes(d[..8].try_into().unwrap())
            }
        }
    }
}

#[derive(Default)]
struct Totals {
    entries: AtomicU64,
    files: AtomicU64,
    bytes: AtomicU64,
    skipped_mounts: AtomicU64,
    checksum: AtomicU64,
}

impl Totals {
    fn file(&self, size: u64, h: u64) {
        self.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(size, Ordering::Relaxed);
        self.checksum.fetch_xor(h, Ordering::Relaxed);
    }
}

thread_local! {
    static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; READ_BUF]);
}

fn hash_fd(fd: &OwnedFd, hash: Hash) -> u64 {
    BUF.with_borrow_mut(|buf| {
        hash.stream(
            |b| loop {
                match rustix::io::read(fd, &mut *b) {
                    Ok(n) => break n,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(e) => panic!("read: {e}"),
                }
            },
            buf,
        )
    })
}

fn hash_file(file: &mut File, hash: Hash) -> u64 {
    BUF.with_borrow_mut(|buf| hash.stream(|b| file.read(b).expect("read"), buf))
}

fn walk_fd(root: &Path, threads: usize, hash: Hash, t: &Totals) {
    let root_fd = rustix::fs::open(
        root,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .expect("open root");
    let root_mnt = rustix::fs::statx(&root_fd, c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .expect("statx root")
        .stx_mnt_id;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    pool.scope(|s| dir_task(s, root_fd, root_mnt, hash, t));
}

fn dir_task<'s>(s: &rayon::Scope<'s>, dir: OwnedFd, root_mnt: u64, hash: Hash, t: &'s Totals) {
    let mut dbuf = [MaybeUninit::<u8>::uninit(); DIR_BUF];
    let mut iter = RawDir::new(dir.as_fd(), &mut dbuf);
    while let Some(entry) = iter.next() {
        let entry = entry.expect("getdents64");
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        t.entries.fetch_add(1, Ordering::Relaxed);
        entry_work(s, dir.as_fd(), name, root_mnt, hash, t);
    }
}

fn entry_work<'s>(
    s: &rayon::Scope<'s>,
    dir: BorrowedFd<'_>,
    name: &CStr,
    root_mnt: u64,
    hash: Hash,
    t: &'s Totals,
) {
    let st = rustix::fs::statx(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT,
        StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
    )
    .expect("statx");
    if st.stx_mnt_id != root_mnt {
        t.skipped_mounts.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let resolve = ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV;
    match FileType::from_raw_mode(st.stx_mode.into()) {
        FileType::Directory => {
            let child = rustix::fs::openat2(
                dir,
                name,
                OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
                resolve,
            )
            .expect("openat2 dir");
            s.spawn(move |s| dir_task(s, child, root_mnt, hash, t));
        }
        FileType::RegularFile => {
            let h = if hash == Hash::None {
                0
            } else {
                let fd = rustix::fs::openat2(
                    dir,
                    name,
                    OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NOATIME,
                    Mode::empty(),
                    resolve,
                )
                .or_else(|_| {
                    // O_NOATIME needs ownership; fall back without it.
                    rustix::fs::openat2(
                        dir,
                        name,
                        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                        Mode::empty(),
                        resolve,
                    )
                })
                .expect("openat2 file");
                hash_fd(&fd, hash)
            };
            t.file(st.stx_size, h);
        }
        _ => {}
    }
}

fn walk_jwalk(root: &Path, threads: usize, hash: Hash, t: &Totals) {
    let t_ptr = t as *const Totals as usize;
    let walk = jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
        .parallelism(jwalk::Parallelism::RayonNewPool(threads))
        .process_read_dir(move |_, dir, _, children| {
            // SAFETY: `t` outlives the walk, which is drained below.
            let t = unsafe { &*(t_ptr as *const Totals) };
            for child in children.iter().flatten() {
                t.entries.fetch_add(1, Ordering::Relaxed);
                let path = dir.join(&child.file_name);
                let md = std::fs::symlink_metadata(&path).expect("lstat");
                if md.is_file() {
                    let h = if hash == Hash::None {
                        0
                    } else {
                        hash_file(&mut File::open(&path).expect("open"), hash)
                    };
                    t.file(md.len(), h);
                }
            }
        });
    for e in walk {
        e.expect("jwalk");
    }
}

fn read_all(root: &Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for e in jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(jwalk::Parallelism::Serial)
    {
        let e = e.expect("walk");
        if e.file_type().is_file() {
            out.push(std::fs::read(e.path()).expect("read"));
        }
    }
    out
}

fn main() {
    // The fd walk holds one descriptor per queued directory.
    let lim = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    rustix::process::setrlimit(
        rustix::process::Resource::Nofile,
        rustix::process::Rlimit {
            current: lim.maximum,
            maximum: lim.maximum,
        },
    )
    .expect("setrlimit");

    let args: Vec<String> = std::env::args().collect();
    let t = Totals::default();
    match args.get(1).map(String::as_str) {
        Some("walk") => {
            let (walker, root, threads, hash) = (
                args[2].as_str(),
                Path::new(&args[3]),
                args[4].parse().unwrap(),
                Hash::parse(&args[5]),
            );
            let start = Instant::now();
            match walker {
                "fd" => walk_fd(root, threads, hash, &t),
                "jwalk" => walk_jwalk(root, threads, hash, &t),
                _ => panic!("unknown walker {walker}"),
            }
            let secs = start.elapsed().as_secs_f64();
            println!(
                "{{\"bench\":\"walk\",\"walker\":\"{walker}\",\"threads\":{threads},\"hash\":\"{}\",\"secs\":{secs:.4},\"entries\":{},\"files\":{},\"bytes\":{},\"skipped_mounts\":{},\"checksum\":{}}}",
                args[5],
                t.entries.load(Ordering::Relaxed),
                t.files.load(Ordering::Relaxed),
                t.bytes.load(Ordering::Relaxed),
                t.skipped_mounts.load(Ordering::Relaxed),
                t.checksum.load(Ordering::Relaxed),
            );
        }
        Some("mem") => {
            let (root, threads, hash) = (
                Path::new(&args[2]),
                args[3].parse::<usize>().unwrap(),
                Hash::parse(&args[4]),
            );
            let files = read_all(root);
            let bytes: u64 = files.iter().map(|f| f.len() as u64).sum();
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let mut runs = Vec::new();
            for _ in 0..5 {
                let start = Instant::now();
                let sum = pool.install(|| {
                    use rayon::prelude::*;
                    files
                        .par_iter()
                        .map(|f| hash.bytes(f))
                        .reduce(|| 0, |a, b| a ^ b)
                });
                runs.push(start.elapsed().as_secs_f64());
                t.checksum.fetch_xor(sum, Ordering::Relaxed);
            }
            runs.sort_by(f64::total_cmp);
            println!(
                "{{\"bench\":\"mem\",\"threads\":{threads},\"hash\":\"{}\",\"files\":{},\"bytes\":{bytes},\"median_secs\":{:.4},\"min_secs\":{:.4},\"max_secs\":{:.4},\"gib_per_s\":{:.2},\"checksum\":{}}}",
                args[4],
                files.len(),
                runs[2],
                runs[0],
                runs[4],
                bytes as f64 / runs[2] / (1u64 << 30) as f64,
                t.checksum.load(Ordering::Relaxed),
            );
        }
        _ => panic!("usage: see the module docs"),
    }
}
