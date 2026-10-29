# rnk-layer

> **Status:** in progress · **Phase:** 1 ·
> **Depends on:** none yet (`rnk-oci` planned) · **Used by:** `rnk-snapshot` (decoder, writer), `rnk-builder` (`rnk-cache`, `rnk-context` [open: Q-028]) · **Path:** `crates/rnk-layer/`

**TL;DR:** Entries in, deterministic tar out, through parallel gzip or zstd,
with diff ID and digest computed while streaming; and a gzip/zstd decoder
for pulled layers. Same entries, same bytes, whatever the thread count.

## Owns

- Writing layer tar streams with fully controlled headers: the writer sorts
  the entries it is fed and emits missing parent directory headers;
  hardlinks, PAX xattrs, fixed timestamps under `--reproducible`.
- Parallel gzip and zstd compression; decompression of pulled blobs into a
  plain tar `Read` for `rnk-rootfs::extract`; digests while streaming.

## Must not

- Produce bytes that depend on thread count or on how input is chunked.
- Decide what goes into a layer (`rnk-snapshot`: diff, whiteout conversion),
  when to export it (`rnk-builder`) or where it goes (`rnk-registry`); write
  extracted entries to disk (`rnk-rootfs`).

## API (planned)

Built: everything below except zstd [open: Q-052].

```rust
pub struct EntryPath;              // [design] relative, normalized bytes; orders component-wise
pub struct Entry { pub path: EntryPath, pub kind: EntryKind, pub mode: u32 /* raw st_mode, 0o7777 written */,
    pub uid: u64, pub gid: u64, pub mtime: i64 /* seconds */, pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub inode: Option<(u64, u64)> } // [design] (dev, ino) finds hardlinks
pub enum EntryKind { File { size: u64 }, Dir, Symlink(Vec<u8>), Char(u32, u32), Block(u32, u32), Fifo,
    Whiteout /* path = the deleted path */, OpaqueDir }  // [design]
pub enum Compression { None, Gzip, Zstd /* [open: Q-052] */ }
pub struct EncodeOptions { pub compression: Compression, pub level: i32, pub threads: NonZeroUsize, pub mtime: Option<i64> } // [design] mtime: --reproducible
pub trait EntrySource {            // [design] implemented by the exporter
    fn open(&mut self, e: &Entry) -> io::Result<Box<dyn Read + '_>>;            // file content, read once in layer order
    fn parent(&mut self, p: &EntryPath) -> io::Result<Option<Entry>>;          // [open: Q-013] interim: missing parents
}
pub struct LayerWriter;            // [design] new(EncodeOptions); add(Entry) in any order; finish(&mut dyn EntrySource, W) -> LayerDigests
pub struct LayerDigests { pub diff_id: Sha256Digest, pub digest: Sha256Digest, pub size: u64, pub compression: Compression }
pub struct Sha256Digest;           // [design] Display `sha256:<hex>`; [open: Q-012] sha2 interim
pub struct Decoder<R: Read>;       // [design] new(blob) sniffs gzip/zstd/plain; impl Read gives the plain tar;
                                   // diff_id() once at the end, finish() drains and returns it; tar_size()
                                   // [inferred] gzip inflate is sequential: "parallel" means pipelining inflate, hashing, writes
```

- `finish` sorts, looks up missing parents once each (outermost first),
  turns entries sharing an inode into hardlinks to the first in layer order,
  then streams, hashing the tar (diff ID) and the blob as they pass.
  `LayerDigests` carries the compression, not a media type: that needs the
  image format, which `rnk-oci` owns.
- Layer order `[open: Q-053]`: depth first, components byte-wise; in each
  directory the opaque marker, then whiteouts, then the rest. Names starting
  with `.wh.` are rejected.
- Headers: PAX as Go's `archive/tar` writes them (`src/header.rs`); whiteout
  and opaque markers are empty files, mode 0, 0:0, the entry's mtime.
- gzip (`src/gzip.rs`, flate2 on zlib-rs): 128 KiB blocks by offset, each
  deflated alone with the previous 32 KiB as dictionary and a sync flush,
  then an empty final block (`03 00`); header mtime 0, OS 255; CRC combined.
  `threads` = 1 does everything in the caller's thread; above 1, `threads`
  workers compress and two more threads hash the tar and the blob in order,
  so the caller only copies and writes.

## Invariants

- Output depends only on entries, compression, level and block size: not on
  threads, chunking, clock (with `--reproducible`) or host users
  ([ARCHITECTURE.md#determinism](../../../ARCHITECTURE.md#determinism)); gzip is one member, zstd gets 128 KiB chunks.
- Raw mode bits survive (sticky, setuid, setgid); no user or group names.
- A hostile tar never panics the reader and stays bounded in memory
  ([threat model T15, T16](../../security/threat-model.md#5-threats-and-mitigations)).

## Refs

- **Design:** [design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer](../../design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer), [#lazy-export](../../design/overlay-snapshot.md#lazy-export), [design/kaniko-mode.md#exporter-requirements-specific-to-the-walk](../../design/kaniko-mode.md#exporter-requirements-specific-to-the-walk)
- **Decisions:** [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (accepted differences between modes), [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (`--compression`, `--reproducible`), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (C zstd via zig)
- **Evidence:** [exp-05 Q3](../../research/experiments/05-rust-ecosystem.md#q3-parallel-compression) (pigz-style gzip 2.2 GB/s at 32
  threads, zstd 4.2 GB/s at 16), [Q5](../../research/experiments/05-rust-ecosystem.md#q5-the-tar-crate-0446) (`tar` crate); [exp-07](../../research/experiments/07-kaniko-vs-overlay.md#image-check-1)
  (parent headers, raw modes); [exp-08](../../research/experiments/08-parallel-stages-lazy-export.md#lazy-export-and-skipped-snapshots) (lazy export saves 1.3–6.6 s).
- **kaniko:** `kaniko@44aa7ddc0 pkg/util/tar_util.go:61-129` (PAX, no
  uname/gname, trailing `/`, no sockets), `:148-160` (only
  `security.capability`), `:162-176` (whiteout), `:179-197` (hardlinks);
  `pkg/snapshot/snapshot.go:341-390`, `:432-446` (parent directories);
  `pkg/pgzip/pgzip.go:17-35` (local branch only: 1 MiB independent blocks)
- **External:** [OCI image-spec v1.1.1 layer.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/layer.md); BuildKit v0.34.0 [WriteUpperdir](https://github.com/moby/buildkit/blob/v0.34.0/util/overlay/overlay_linux.go#L113), [sameDirent](https://github.com/moby/buildkit/blob/v0.34.0/util/overlay/overlay_linux.go#L348); [tar 0.4.46](https://docs.rs/tar/0.4.46/tar/)

## Tests

- T0 (`tests/it/`, unit tests in `src/`): pinned sha256s of a sample tree,
  plain and gzip, checked with GNU tar and gzip; same bytes for threads 1..16
  and random chunkings (proptest); random trees round-tripped through the
  `tar` crate (124-byte components, long link targets, PAX xattrs, hardlinks,
  raw modes, whiteouts, opaque directories, uid/gid > 2,097,151, negative
  mtimes); a > 8 GiB header; gzip is one member; decoder round trips and
  hostile input. Still to add with zstd: its goldens and 20 runs at top
  threads.
- T1/T2: image content against kaniko ([testing](../../testing.md#planned-test-suites)).

## Pitfalls

- Never `tar::Builder::append_dir_all` or `append_path`: unsorted, no
  hardlinks, modes forced to 644/755, no xattrs ([exp-05 Q5](../../research/experiments/05-rust-ecosystem.md#q5-the-tar-crate-0446)).
- `gzp` gives other bytes at t=1 than at t≥2 and is nondeterministic with a
  dictionary; pigz-style (128 KiB blocks, 32 KiB dictionary, sync flush,
  `Crc::combine`) is deterministic at every t ([exp-05 Q3](../../research/experiments/05-rust-ecosystem.md#q3-parallel-compression)).
- zstd at t=0 and t≥1 differ, and one 1 GiB write at t=32 gave two results:
  always multithreaded mode with 128 KiB writes, even for one worker. Library,
  level and block size are part of every digest (Q3).
- Overlay whiteouts share one inode (`nlink=4`): detect hardlinks only after
  whiteouts are converted ([design](../../design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer)); a hardlink whose target is
  in a lower layer is written as a regular file ([inferred](../../design/kaniko-mode.md#exporter-requirements-specific-to-the-walk)).
- Without parent directory headers, podman made `/root` (700) and `/tmp`
  (1777) 755; Go-style mode handling made `/tmp` 777 ([exp-07](../../research/experiments/07-kaniko-vs-overlay.md#image-check-1)).
- Exports after page reclaim re-read 124–466 MB from disk: export right after
  the layer, with capped threads ([exp-07](../../research/experiments/07-kaniko-vs-overlay.md#overlay-export-variance-re-reading-from-disk)).

## Phase 1

- All of it ([survey 04 §2](../../research/survey/04-risks-and-roadmap.md#2-mvp-and-10-scope)): parallel gzip and zstd, tar streamed
  into the compressor, `--reproducible`, and a reader for base layers.
- Order: tar writer with goldens → gzip and zstd with determinism tests →
  streaming digests → decoder → exporter integration with `rnk-snapshot`.
- [open: Q-013] How `LayerWriter` gets a missing parent's metadata from
  the exporter; interim: `EntrySource::parent`.
- Done: tar writer with goldens; parallel gzip with determinism tests;
  streaming digests; the decoder (gzip with any number of members, plain)
  (`tests/it/`); `examples/throughput.rs` prints MB/s per thread count.
- Left: zstd ([open: Q-052]); exporter integration with `rnk-snapshot`
  (task 9).
