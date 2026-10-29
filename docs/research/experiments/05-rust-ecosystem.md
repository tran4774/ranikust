# Experiment 05: Rust ecosystem

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Do the Rust ecosystem assumptions of the survey hold: a static musl binary with rustls, aarch64 cross builds, allocator speed, deterministic parallel compression, and usable OCI, tar, credential and CPU-count crates? |
| **Answer** | Mostly. A static-pie musl binary is 5.1 MB (ring) or 7.3 MB (aws-lc) and cross-builds to aarch64 with cargo-zigbuild; but musl's default allocator is about 250–300 times slower than mimalloc or glibc at 16 threads, reqwest 0.13 needs a hand-built rustls config to find CA certificates in `scratch`, and only dictionary-free parallel gzip is deterministic, while multithreaded zstd is not reliably so. |
| **Related** | [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md), [ADR-0009](../../adr/0009-x86-64-first-aarch64-ready.md), [ADR-0013](../../adr/0013-jobs-default-available-parallelism.md); survey chapters [03](../survey/03-rust-architecture.md) and [05](../survey/05-decisions.md); experiment [16](16-cgroup-v1-cpuset.md) |
| **Original** | `docs-rust/poc/05-rust-ecosystem/` (Vietnamese, not in this repository) |

## Summary

- A static musl x86_64 binary with tokio, reqwest (rustls), zstd, flate2 and
  sha2 builds without OpenSSL and runs in an empty rootfs; it cross-builds
  to aarch64 musl with cargo-zigbuild and runs under qemu-user.
- reqwest 0.13 dropped its `webpki-roots` feature: by default it reads the
  system CA store and fails to build a client in `scratch`. About 15 lines
  that build a `rustls::ClientConfig` from webpki roots plus host CAs fix it.
- musl's default allocator collapses under threads (16 threads of small
  allocations: 2.8 million ops/s against about 700 with mimalloc and 900
  with glibc). mimalloc is mandatory for a musl build.
- Parallel gzip without a dictionary (`gzp`, or a hand-written pigz-style
  compressor) is single-member and deterministic; `gzp` with a dictionary is
  not. Multithreaded zstd output depends on how data is fed to the encoder.
- `oci-client` 0.18.0 streams pulls and pushes in about 20 MB RSS and
  supports cross-repo mount, but has no retry, no Docker config auth, and
  treats a refused mount as an error. The `tar` crate writes deterministic
  PAX layers with hand-built headers. `available_parallelism()` honours both
  cgroup v2 quota and CPU affinity.

## Questions

The experiment checked assumptions in
[survey chapter 03](../survey/03-rust-architecture.md) §1 and §3, and
[survey chapter 05](../survey/05-decisions.md) §1–§2 (section numbers refer
to the original chapters):

1. Can ranikust ship a static musl binary using rustls and no OpenSSL, and
   cross-build it for `aarch64-unknown-linux-musl`?
2. Is musl's allocator slow under multithreading?
3. Does `gzp` give parallel, single-member, deterministic gzip, and is
   multithreaded `zstd` deterministic for a given thread count?
4. Does `oci-client` pull and push as streams, and support blob mount?
5. Is the `tar` crate enough, or must headers be written by hand?
6. Does `available_parallelism()` honour CPU quota and affinity?
7. Are `docker_credential` and `oci-spec` usable?

## Environment

- Development machine (see the [research README](../README.md#machine)):
  32 CPUs, Linux 7.0.0-34-generic, host glibc 2.43, podman rootless,
  cgroup v2.
- Rust 1.99.1 in containers only (`rust:1-alpine` for musl,
  `rust:1-bookworm` for glibc and cross builds); Rust was not installed on
  the host.
- Release profile: `lto = "thin"`, `strip = true`, `panic = "abort"`.
- The allocator and compression benchmarks ran on the host with the built
  binaries, without a CPU limit; compression was measured in memory, with no
  disk writes. Machine load was not recorded.
- Raw output: `out/*.txt` in the original PoC directory.

## Method

A Cargo workspace with one crate per question:

| Crate | Question | Content |
|---|---|---|
| `crates/static-net` | 1 | tokio + reqwest (rustls) + zstd + flate2 (zlib-rs) + sha2. Feature `aws-lc` or `ring` |
| `crates/allocbench` | 2 | Three workloads: small allocations; path strings + `HashMap`; gzip in 1 MiB blocks. Feature `mimalloc` |
| `crates/pargz` | 3 | `gzp`, `gzp` forced parallel, a hand-written pigz-style compressor, multithreaded zstd. Feature `dict` (`gzp` + zlib-ng) |
| `crates/ocipoc` | 4, 7 | pull, push in four modes, pull blob, mount, credentials |
| `crates/tarpoc` | 5 | Write a layer (gnu/pax), read, probe (stat + xattr), `append_dir_all` |
| `crates/nproc` | 6 | Print `available_parallelism()`, `cpu.max`, `Cpus_allowed_list` |

Crate versions (from `Cargo.lock`): reqwest 0.13.5, rustls 0.23.46,
aws-lc-rs 1.19.0, ring 0.17.14, webpki-roots 1.0.11, tokio 1.54.1,
flate2 1.1.10, zlib-rs 0.6.8, zstd 0.14.3 (libzstd 1.5.7), sha2 0.11.1,
mimalloc 0.1.52, gzp 2.0.4, libz-ng-sys 1.1.31, oci-client 0.18.0,
oci-spec 0.10.0, docker_credential 1.4.0, tar 0.4.46.

Test data for questions 2 and 3: `out/data.bin`, the first 1 GiB of a tar
of `/usr/local/rustup /usr/lib /usr/share` from `rust:1-bookworm`. It
resembles a real layer (`.so`, `.rlib`, text); `gzip -6` compresses it to
33 %.

Per-question method:

- **Q1.** Build `static-net` for `x86_64-unknown-linux-musl` with each TLS
  provider, inspect it with `file`, `ldd` and `cargo tree`, and call
  `https://registry-1.docker.io/v2/` (expecting 401) on the host, in an
  empty rootfs (≈ `FROM scratch`), in an empty rootfs with the host's
  `/etc/ssl/certs` mounted or `SSL_CERT_FILE` set, and in busybox. Two CA
  modes: **platform** (reqwest's default) and **bundled** (webpki roots +
  host CAs). Cross-build for aarch64 with cargo-zigbuild 0.24.0 + zig 0.15.2
  and run under `qemu-aarch64-static`.
- **Q2.** Four variants of the same program (musl, musl + mimalloc, glibc,
  glibc + mimalloc), three workloads, 1–32 threads; each cell run twice and
  reported as a range.
- **Q3.** Compress `data.bin` at level 6 (gzip) and level 3 (zstd) with
  1–32 threads, binary built with musl + mimalloc; compare against pigz 2.6
  and GNU gzip 1.12 from Debian bookworm. "Single member" is checked with
  `flate2::bufread::GzDecoder`, which reads only the first member: it must
  return the full 1 GiB with matching content and no trailing bytes.
  Determinism is checked by comparing sizes and sha256 across runs; zstd is
  repeated 20 times per setting. Ranges span the runs reported in the
  original; the number of runs per cell was not recorded except where
  stated.
- **Q4.** A local `registry:2` on `127.0.0.1:5055` (HTTP) and a random
  500 MB blob; each operation in its own process so that peak RSS
  (`VmHWM`) can be read. Plus a pull of `alpine:latest` from docker.io, and
  a reading of `oci-client`'s `src/client.rs`.
- **Q5.** Write a test layer twice with each of two writers and compare
  sha256; read it back with tar-rs, GNU tar 1.34 and `podman load`.
- **Q6.** Print `available_parallelism()` from a static musl binary in an
  empty rootfs under rootless podman, across CPU quota and affinity
  settings.
- **Q7.** Registry metadata for both crates, and `docker_credential` 1.4.0
  against a fake `config.json` with fake credential helpers.

## Results

All results are **[measured]** unless labelled otherwise.

Table 1. Overview.

| # | Assumption | Verdict | Key figures |
|---|---|---|---|
| 1 | Static musl binary, rustls, no OpenSSL | **Holds**, but the CA handling **needs correction** | static-pie, 5.1 MB (ring) / 7.3 MB (aws-lc). Runs in an empty rootfs |
| 1 | "rustls with webpki-roots" | **Needs correction** | reqwest 0.13 dropped the `webpki-roots` feature. By default it reads the system CAs, and in `scratch` it fails when the client is built. A `rustls::ClientConfig` must be built by hand |
| 1 | Cross build for `aarch64-unknown-linux-musl` | **Holds** | `cargo-zigbuild` 0.24 + zig 0.15.2: builds, runs correctly under `qemu-aarch64-static` |
| 2 | musl's allocator is slow with many threads | **Holds, worse than expected** | 16 threads of small allocations: musl 2.8 Mops/s, musl + mimalloc 700, glibc 900. About **250–300 times** slower. More threads are slower than one |
| 3 | `gzp` gives parallel, single-member, deterministic gzip | **Partly holds** | Without a dictionary: one member, deterministic for every t ≥ 2 (t = 1 differs), 1.6 GB/s at 16 threads. With a dictionary: **not deterministic**, different on every run |
| 3 | Multithreaded `zstd` is deterministic for a given thread count | **Needs correction** | 4.2 GB/s at 16 threads. The output depends on how data is written into the encoder: with a single 1 GiB `write_all`, t = 32 gave 2 different results (7 of 20 runs) |
| 4 | `oci-client` pulls and pushes; streaming and mount need checking | **Mostly holds** | 0.18.0. Streams a pull to a file in 20 MB RSS; streams a 500 MB push in 20 MB RSS. Has mount. Does not read Docker config, does not retry, and reports a refused mount as an error |
| 5 | The `tar` crate needs checking; headers may have to be written by hand | **Holds** | PAX xattrs, hardlinks, a 232-character path, fixed mtime: deterministic, read correctly by GNU tar and `podman load`. `append_dir_all` is unusable: no sorting, no hardlinks, modes forced to 644/755 |
| 6 | `available_parallelism()` honours quota and affinity | **Holds** | `--cpus=2` → 2, `--cpus=2.5` → 2, `--cpus=0.5` → 1, affinity 0-3 → 4, `--cpus=2` + affinity 0-3 → 2 |
| 7 | `docker_credential`, `oci-spec` | **Usable** | `docker_credential` 1.4.0 (2026-05): small, few maintainers. `oci-spec` 0.10.0 (2026-05): actively maintained |

### Q1: static binary

Table 2. x86_64 musl builds (`out/q1-build.txt`).

| rustls provider | `file` | `ldd` (host glibc) | Size (stripped) | Build (warm cache) |
|---|---|---|---|---|
| aws-lc-rs (reqwest 0.13's default) | static-pie linked | statically linked | 7,271,488 B | 26 s |
| ring | static-pie linked | statically linked | 5,149,504 B | 25 s |

- `cargo tree -i openssl-sys`: absent from the dependency tree.
- `cargo tree -i zlib-rs`: flate2 uses the `zlib-rs` backend.
- musl's `ldd` in Alpine prints `/lib/ld-musl-x86_64.so.1` for a static-pie
  file. That is how musl's `ldd` displays it, not a real dependency: the
  host's glibc `ldd` reports `statically linked`, and the binary runs in a
  rootfs containing nothing else.
- aws-lc-sys builds on Alpine with only the stock `gcc` + `musl-dev`; no
  cmake or perl is needed.

**CA certificates** (`out/q1-run.txt`). reqwest 0.13 changed its TLS
handling: the `rustls` feature uses `rustls-platform-verifier`, which reads
the system CAs, and the `rustls-tls-webpki-roots` feature is gone.

Table 3. Calling `https://registry-1.docker.io/v2/` (401 expected).

| Environment | `platform` (reqwest default) | `bundled` (webpki-roots + host CAs) |
|---|---|---|
| Host | 401 OK | 401 OK (118 webpki + 121 host) |
| Empty rootfs (≈ `FROM scratch`) | **Fails when building the client: `No CA certificates were loaded from the system`** | 401 OK (118 webpki + 0) |
| scratch + mounted `/etc/ssl/certs` | 401 OK | 401 OK |
| scratch + `SSL_CERT_FILE` | 401 OK | 401 OK |
| busybox (no CAs) | Fails as above | 401 OK |

Both providers gave the same results.

The `bundled` approach (`crates/static-net/src/main.rs`): build a
`rustls::RootCertStore` from `webpki_roots::TLS_SERVER_ROOTS`, add
`rustls_native_certs::load_native_certs()` (which reads `/etc/ssl/certs`,
`SSL_CERT_FILE`, `SSL_CERT_DIR`), then call
`ClientBuilder::tls_backend_preconfigured(config)`. About 15 lines.

Caveat (**[inferred]**, not tested in the PoC): `oci-client` creates its own
`reqwest::Client` internally and accepts only `tls_certs_only` /
`extra_root_certificates`, not a ready-made `ClientConfig`. With
`oci-client`, the CA list (webpki + host) must therefore be passed through
`tls_certs_only`. That needs the `webpki-root-certs` crate (full DER
certificates) instead of `webpki-roots` (trust anchors only).

Table 4. aarch64 cross build (`out/q1-aarch64.txt`), with `cargo-zigbuild`
0.24.0 and zig 0.15.2 in `rust:1-bookworm`; no separate musl cross toolchain
was needed.

| Provider | Result | Size | `file` | Run under `qemu-aarch64-static` |
|---|---|---|---|---|
| ring | OK | 4.0 MB | ARM aarch64, statically linked | 401 OK, gzip + zstd OK |
| aws-lc-rs | OK | 5.4 MB | ARM aarch64, statically linked | 401 OK, gzip + zstd OK |

The C code (zstd-sys, aws-lc-sys, ring) cross-builds through `zig cc`.
`cargo install cargo-zigbuild` takes about 1 minute; zig is a 50 MB
tarball.

### Q2: allocator

`out/q2-run.txt`. Run on the host; each cell run twice, range shown. No CPU
limit.

Table 5. Workload `small`: each thread does 4 million alloc/free pairs of
random size 8–1024 B, keeping 1024 blocks live. Unit: million ops/s (total).

| Variant | 1 thread | 4 | 16 | 32 |
|---|---|---|---|---|
| musl (mallocng) | 16–21 | 7–10 | **2.8–2.9** | **2.5–2.6** |
| musl + mimalloc | 62–64 | 184–221 | 663–753 | 858–879 |
| glibc | 78–79 | 301–302 | 793–1004 | 1171–1228 |
| glibc + mimalloc | 79 | 260–276 | 787–864 | 1028–1036 |

Table 6. Workload `paths` (like a snapshot: `format!` a path, `replace`,
insert into a `HashMap`; 1 million per thread). Unit: million ops/s.

| Variant | 1 | 4 | 16 | 32 |
|---|---|---|---|---|
| musl | 2.0 | 1.8–1.9 | **0.8–0.9** | **0.9** |
| musl + mimalloc | 3.6–3.9 | 12.4–12.7 | 32–36 | 37–39 |
| glibc | 3.4 | 9.6–9.7 | 24.4–24.7 | 26.2 |
| glibc + mimalloc | 7.2–7.3 | 17.7–18.8 | 40–41 | 41–42 |

Table 7. Workload `gzip` (flate2/zlib-rs level 6, 1 MiB blocks, 1 GiB of
data). Unit: MB/s.

| Variant | 1 | 16 | 32 |
|---|---|---|---|
| musl | 120 | 1736–1753 | 2565–2590 |
| musl + mimalloc | 122 | 1772–1805 | 2626–2627 |
| glibc | 121–122 | 1764–1786 | 2621–2628 |
| glibc + mimalloc | 121 | 1769–1792 | 2594–2598 |

- musl's default allocator is not just slow: **it gets slower as threads
  are added**. 16 threads are 7 times slower than 1 thread (`small`) and
  2.4 times slower (`paths`). At 16 threads, `small` takes 22 s against
  0.08 s with mimalloc.
- With mimalloc, musl matches glibc on `small` and beats glibc on `paths`.
- The compression workload with large blocks is unaffected: few
  allocations, time spent in compression.
- Snapshotting, walking and hashing many small files means many small
  allocations, exactly where musl collapses. Conclusion: mimalloc (or
  jemalloc) is **mandatory** for the musl build. mimalloc builds statically
  with musl with nothing extra and adds about 150 KB to the binary.
- jemalloc (`tikv-jemallocator`) was not measured.

### Q3: parallel compression

`out/q3-run.txt`. 1 GiB `data.bin`; gzip level 6, zstd level 3; in memory,
no disk writes; binary built with musl + mimalloc; pigz 2.6 and gzip from
Debian bookworm. No CPU limit.

Table 8. gzip.

| Method | t=1 | t=4 | t=16 | t=32 | Output (bytes) | 1 member? | Deterministic across t? |
|---|---|---|---|---|---|---|---|
| flate2 (zlib-rs), single thread | 113–120 MB/s | | | | 367,697,026 | yes | — |
| `gzp` (no dictionary) | 120 | 438–470 | 1396–1579 | 1965–2052 | 372,927,311 (t ≥ 2) | yes | t ≥ 2 identical. **t = 1 differs** (takes the `SyncZ` path, same as flate2) |
| `gzp` forced to `ParCompress` even at t = 1 | 124 | 452–471 | 1527–1647 | 1985–2025 | 372,927,311 | yes | **yes**, every t, repeated many times |
| `gzp` + dictionary (zlib-ng) | 119–123 | 446–467 | 1437–1591 | 1862–2108 | ≈ 367,282,2xx | yes | **no**. Size and sha256 change on every run, even at the same t |
| Hand-written, pigz-style (zlib-rs) | 115–118 | 437–445 | 1548–1602 | 2216–2234 | 367,282,454 | yes | **yes**, every t, repeated twice |
| pigz 2.6 | 51–53 | 200–201 | 697–714 | 945–981 | 357,598,363 | yes | yes |
| GNU gzip 1.12 | 43 | | | | 357,949,759 | yes | — |

- The t = 16 outputs of forced-parallel `gzp` and of the hand-written
  compressor also pass `gzip -t`, and `zcat | sha256sum` matches the
  original.
- `gzp` without a dictionary compresses each 128 KiB block independently;
  the ratio is about 1.4 % worse.
- `gzp` enables the dictionary only with a C zlib (`check.rs` uses
  `libz_sys::adler32_combine`). Combining `gzp/any_zlib` with flate2's
  `zlib-rs` **does not compile**; `deflate_zlib_ng` (C, needs cmake) is
  required.
- The dictionary variant is not deterministic. **[inferred]** Likely cause,
  from the source and not verified: each thread reuses one `Compress`,
  calling `set_dictionary` and then `reset()` after compressing. The
  hand-written version creates a new `Compress` per block and is
  deterministic.
- The hand-written pigz-style compressor (`pigz_like` in
  `crates/pargz/src/main.rs`, about 45 lines): 128 KiB blocks, a 32 KiB
  dictionary from the previous block, `FlushCompress::Sync`, `Finish` on the
  last block, per-block CRC merged with `Crc::combine`. Pure Rust,
  deterministic, compresses as well as `gzp` with a dictionary, and is the
  fastest at 32 threads.
- zlib-rs at level 6 is about 2.3 times faster per thread than pigz's zlib,
  but its output is about 2.7 % larger.
- pigz, `gzp` and the hand-written compressor all produce different bytes.
  A layer digest is stable only with the library, level and block size
  fixed.

Table 9. zstd (crate `zstd` 0.14.3, libzstd 1.5.7, feature `zstdmt`).

| t | 0 (single thread) | 1 | 2 | 4 | 16 | 32 |
|---|---|---|---|---|---|---|
| MB/s | 449–457 | 498–523 | 986–1018 | 1705–1873 | 3667–4325 | 4197–4728 |
| Output (bytes) | 342,666,159 | 340,611,215 | 340,611,215 | 340,611,215 | 340,611,215 | 340,611,215 **or 340,611,212** |

- t = 0 (single-threaded) and t ≥ 1 (multithreaded mode) give different
  output. This is known libzstd behaviour.
- One `write_all` of 1 GiB at t = 32: 20 runs gave 2 results (13 runs
  `a64d…`, 7 runs `36bf…`). At t = 8: 20/20 identical.
- Writing in 128 KiB chunks: 20/20 identical at t = 32, and identical for
  every t from 1 to 32.
- Writing in 1 MiB chunks at t = 32: the first measurement gave 20/20
  `36bf…` (different from t ≤ 16); the second gave 18 `36bf…` + 2 `a64d…`.
  Also not deterministic.
- The host's `zstd` CLI 1.5.2 (`-3 -T8` and `-T32`, 10 runs each): always
  identical.
- Conclusion: multithreaded zstd is **not guaranteed deterministic**; the
  output depends on the thread count and on how data is fed in. For a
  stable digest, either feed fixed 128 KiB chunks (= `ZSTD_CStreamInSize`)
  and keep a repetition test, or split the data into independent frames.
  Reading `zstdmt_compress.c` would settle it (not done).

### Q4: `oci-client` 0.18.0

`out/q4-run.txt`. Local `registry:2` on `127.0.0.1:5055`, HTTP; a random
500 MB blob; each operation in its own process so that peak RSS (`VmHWM`)
can be read.

Table 10. Operations.

| Operation | API | Time | Throughput | Peak RSS |
|---|---|---|---|---|
| Pull `alpine:latest` from docker.io (1 layer, 3.8 MB) | `pull_image_manifest` + `pull_blob` → `tokio::fs::File` | 2.6 s | — | 20 MB |
| Pull a 500 MB blob from the local registry | `pull_blob` → file | 0.42 s | 1253 MB/s | 20 MB |
| Push 500 MB as `Bytes` | `push_blob(Vec<u8>)` (4 MiB chunks) | 3.7 s | 142 MB/s | **526 MB** |
| Push 500 MB, streamed | `push_blob_stream` (one PATCH per 1 MiB item) | 13.0 s | 40 MB/s | 20 MB |
| Push 500 MB, streamed, digest computed on the fly | `push_blob_stream_chunked` | 20.4 s | 26 MB/s | 20 MB |
| Push 500 MB, streamed as one PUT | `use_monolithic_push = true` + `push_blob_stream(size)` | 0.9 s | 575 MB/s | 20 MB |
| Cross-repository mount | `mount_blob` | 0.04 s | — | — |

Reading the source (`oci-client@0.18.0 src/client.rs`, **[code]**):

- **Pull**: `pull_blob<T: AsyncWrite>` reads `bytes_stream()` and writes to
  the writer incrementally, checking the digest as it reads. There are also
  `pull_blob_stream` and `pull_blob_stream_partial` (Range requests, to
  resume after a break). `Client::pull()` alone collects every layer into a
  `Vec<u8>`; do not use it for large layers.
- **Streaming push**: yes. `push_blob_stream` (needs the digest up front)
  and `push_blob_stream_chunked` (computes sha256 while sending). PATCH
  requests are sent **sequentially**, each chunk `min(item, 4 MiB)`.
  `push_chunk_size` is a private field and cannot be changed. With 1 MiB
  items every PATCH is only 1 MiB, which is why chunked mode is slow. A
  monolithic PUT with a streamed body is the fastest and does not hold the
  blob in memory.
- **Mount**: `mount_blob(dst, src, digest)`. When the registry cannot mount,
  it answers 202 with a new upload session; `oci-client` treats that as a
  `SpecViolationError` and drops the session. The caller must fall back to a
  push itself.
- **Auth**: `RegistryAuth::{Anonymous, Basic, Bearer}`. It performs the
  token flow from `WWW-Authenticate` and caches tokens. It does **not** read
  `~/.docker/config.json` or call credential helpers; pair it with
  `docker_credential` (Q7).
- **Missing**: retry, resuming an interrupted upload, parallel upload of
  chunks of one blob, a mirror list (only `oci-spec`'s
  `Reference::set_mirror_registry`: one mirror per reference, no fallback).
- **Present**: `push_manifest_raw` (keeps the manifest bytes, so the digest
  is stable), `blob_exists`, `pull_referrers`,
  `max_concurrent_upload/download` (default 16), proxy, `tls_certs_only`,
  `extra_root_certificates`, `HttpsExcept` for HTTP registries.
- Size: 7,064 lines in `src/` (including 55 tests). Repository
  `oras-project/rust-oci-client`, last push 2026-10-23 (**[external]**).

**[inferred]** Estimate: use `oci-client` for the first version, with a
wrapper of about **400–600 lines**: auth from the Docker config, retry with
backoff, fallback when a mount fails, mirrors with fallback, monolithic PUT
for blobs of known size. If `rk-registry` were written from scratch, the
1,500–2,000-line estimate in survey chapter 03 is still reasonable
(**[inferred]**); most of the effort is the token flow, per-registry error
handling, and tests.

### Q5: the `tar` crate 0.4.46

`out/q5-run.txt`. The test layer contains: a directory; a file with PAX
`SCHILY.xattr.security.capability` (cap_net_raw, 20 B) and
`SCHILY.xattr.user.rk`; a hardlink; a 232-character path (with one
124-character component, so it cannot be split into a ustar prefix); a
symlink with a 142-character target; a UTF-8 name; `etc/.wh.removed.conf`;
`var/.wh..wh..opq`. Every header: mtime 1700000000, uid/gid 0.

Two writers:

- **gnu**: `Header::new_gnu` + `append_data`/`append_link` (the crate
  handles long names with GNU `././@LongLink`); xattrs through
  `append_pax_extensions`.
- **pax**: `Header::new_ustar`, writing PAX `path`/`linkpath` records by
  hand when `set_path` fails (about 25 lines, function `pax_entry`).

Table 11. Checks.

| Check | gnu | pax |
|---|---|---|
| Write twice, compare sha256 | identical | identical |
| tar-rs reads back: long path, long link, xattrs, whiteouts | correct | correct |
| GNU tar 1.34 `-tv --xattrs` | correct (`*` on files with xattrs) | correct |
| GNU tar `-x --xattrs` (root in a user namespace), then `lstat`/`lgetxattr` | hardlink nlink=2 on one inode, 20 B capability, `user.rk=hello`, long path and link correct | as gnu |
| `podman load` (hand-built docker-archive), then run `probe` in the container | as above. The `.wh.*` files do not appear in the rootfs | as gnu |

- Reading whiteouts: the `tar` crate returns `.wh.*` as ordinary files.
  Recognising whiteouts is the caller's job (a few lines).
- `append_dir_all` + `HeaderMode::Deterministic` (reading a real
  directory): two directories with different mtimes give the same sha256.
  But:
  - entry order follows `read_dir`, unsorted; deterministic only if the
    filesystem returns the same order;
  - hardlinks are written as two regular files (no `(dev, ino)` tracking);
  - modes are forced: a 0640 file becomes 0644, a symlink 0755; mtime
    becomes 1153704088;
  - xattrs are not read from the filesystem.
- Conclusion: use `tar::Builder::append` / `append_pax_extensions` with
  hand-built headers, and do the walk, sorting, hardlink map, xattr reading
  and mode handling ourselves. Do not use `append_dir_all`/`append_path`.
  The crate is enough for deterministic PAX output.
- Not tested: sparse files, files > 8 GiB (PAX `size`), uid/gid > 2097151,
  reading tars produced by docker or BuildKit.

### Q6: `available_parallelism()`

`out/q6-run.txt`. Rust 1.99.1, static musl binary, run in an empty rootfs
under rootless podman (cgroup v2).

Table 12. Result by condition.

| Condition | `cpu.max` | `Cpus_allowed_list` | Result |
|---|---|---|---|
| Host | — | 0-31 | 32 |
| Host, `taskset -c 0-5` | — | 0-5 | 6 |
| `podman run` | max | 0-31 | 32 |
| `--cpus=2` | 200000 100000 | 0-31 | 2 |
| `--cpus=2.5` | 250000 100000 | 0-31 | 2 (rounded down) |
| `--cpus=0.5` | 50000 100000 | 0-31 | 1 (minimum 1) |
| `--cpu-quota=150000 --cpu-period=100000` | 150000 100000 | 0-31 | 1 |
| `--cpu-shares=512` | max | 0-31 | 32 (shares ignored, as intended) |
| busybox `taskset -c 0-3` in the container | max | 0-3 | 4 |
| `--cpus=2` + `taskset -c 0-3` | 200000 100000 | 0-3 | 2 (the minimum) |
| `--cpus=8` + `taskset -c 0-3` | 800000 100000 | 0-3 | 4 (the minimum) |
| `--cpuset-cpus=0-3` | — | — | Rootless podman errors out: the `cpuset` controller is not delegated (only `cpu memory pids`) |

- Conclusion: both cgroup v2 quota and affinity are honoured, and the
  smaller wins. Fractional quotas are rounded down.
- `--cpuset-cpus` does not work on this machine (rootless); `taskset`
  inside the container was used instead. **[inferred]** Both go through
  `sched_getaffinity`, so the result should be equivalent.
- cgroup v1 was not tested.

### Q7: `docker_credential` and `oci-spec`

Table 13. Crate status (**[external]**, crates.io and GitHub, 2026-10-29).

| Crate | Latest | Date | Repository | Status |
|---|---|---|---|---|
| `docker_credential` | 1.4.0 | 2026-05-19 | keirlawson/docker_credential, 13 stars, 3 open issues | Low activity, one person. Earlier releases: 1.3.3 (2026-05), 1.3.2 (2025-05), 1.3.1 (2024-03) |
| `oci-spec` | 0.10.0 | 2026-05-27 | youki-dev/oci-spec-rs, 300 stars, push 2026-10-23 | Actively maintained. 0.9.0 (2026-02), 0.8.x (2025). `oci-client` 0.18 depends on `oci-spec` 0.10 |

`docker_credential` 1.4.0 is about 650 lines including tests. Results
against a fake `config.json` (`out/q4-run.txt`):

Table 14. Credential lookup.

| Server asked | Source in the config | Result |
|---|---|---|
| `localhost:5055` | `auths.auth` (base64) | Correct user/password |
| `index.docker.io`, `docker.io`, `registry-1.docker.io` | `auths["https://index.docker.io/v1/"]` | All three correct |
| `ghcr.example.com` | `credHelpers` → `docker-credential-fake` | User/secret from the helper |
| `tok.example.com` | Helper returns `Username: "<token>"` | `IdentityToken` |
| `other.example.com` | `credsStore` | From the default helper |

- Reads `$DOCKER_CONFIG/config.json` or `~/.docker/config.json`. Has
  `get_podman_credential` (reads `REGISTRY_AUTH_FILE` and
  `$XDG_RUNTIME_DIR/containers/auth.json`).
- Synchronous API; helpers are called with `std::process::Command`, so
  under tokio it must go through `spawn_blocking`.
- Only `get`; no `store`/`erase`. Helper calls have no timeout.
- Conclusion: usable. If the crate stops being maintained, rewrite it in
  about 300 lines, as survey chapter 03 estimated.

## Findings

1. A static-pie musl binary with rustls and no OpenSSL is 7,271,488 B with
   aws-lc-rs and 5,149,504 B with ring, and runs in an empty rootfs
   (Table 2).
2. reqwest 0.13's default CA handling fails in `scratch`; a hand-built
   `ClientConfig` with webpki roots plus host CAs works everywhere tested
   (Table 3).
3. cargo-zigbuild 0.24.0 + zig 0.15.2 cross-builds the same binary for
   aarch64 musl (4.0 MB ring, 5.4 MB aws-lc-rs), and it runs under qemu-user
   (Table 4).
4. musl's default allocator gets slower with more threads, reaching
   2.8–2.9 million ops/s at 16 threads against 663–753 with mimalloc
   (Table 5); mimalloc closes the gap with glibc.
5. Dictionary-free `gzp` (forced parallel) and the hand-written pigz-style
   compressor give single-member, deterministic gzip at every thread count;
   `gzp` with a dictionary does not (Table 8).
6. Multithreaded zstd at t = 32 gave two different outputs over 20 runs with
   a single 1 GiB write and with 1 MiB writes, but was stable with 128 KiB
   writes (Table 9).
7. `oci-client` streams pulls and pushes in about 20 MB RSS; a monolithic
   streamed PUT pushed 500 MB in 0.9 s, while chunked streaming took 13.0 s
   and 20.4 s (Table 10).
8. The `tar` crate writes deterministic PAX layers that GNU tar and
   `podman load` read correctly, as long as headers are built by hand;
   `append_dir_all` is unusable (Table 11).
9. `available_parallelism()` takes the minimum of the cgroup v2 quota
   (rounded down, at least 1) and the affinity mask, and ignores CPU shares
   (Table 12).

## Implications for ranikust

- Release binaries are static musl, cross-built with cargo-zigbuild
  ([ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md)); x86_64
  ships first and aarch64 stays buildable
  ([ADR-0009](../../adr/0009-x86-64-first-aarch64-ready.md)).
- The musl build needs mimalloc (or jemalloc); the default allocator is not
  acceptable for walk, hash and snapshot workloads.
- TLS: build the rustls configuration by hand with webpki roots plus host
  CAs, so that ranikust works in `scratch`-like environments; with
  `oci-client`, pass the CA list through `tls_certs_only` (untested).
- Layer compression must be deterministic: gzip with fixed library, level
  and block size, either dictionary-free `gzp` forced to `ParCompress` or
  the hand-written pigz-style compressor. Multithreaded zstd needs fixed
  128 KiB input chunks and a repetition test, or independent frames.
- Registry: start with `oci-client` plus a 400–600-line wrapper (Docker
  config auth through `docker_credential`, retry, mount fallback, mirrors,
  monolithic PUT).
- Tar: build headers by hand with the `tar` crate; walk, sort, hardlinks,
  xattrs and modes are ranikust's own code.
- `--jobs` defaults to `available_parallelism()`
  ([ADR-0013](../../adr/0013-jobs-default-available-parallelism.md)), which
  already honours cgroup v2 quota and affinity.

## Limitations and open questions

- jemalloc was not measured, and no allocator was measured on ranikust's real
  workload (walk + hash + tar).
- `oci-client` was not tested against registries with real auth (htpasswd,
  ECR, GCR), against docker.io for pushes, or against a registry that fails
  mid-transfer.
- Passing the bundled CA list (webpki + host) to `oci-client` through
  `tls_certs_only` was not tested.
- aarch64 unit tests were not run under qemu in CI; only one binary was run.
- The root causes of non-determinism in `gzp` with a dictionary and in
  multithreaded zstd were not established.
- `highway`, `blake3`, `jwalk`, `parse-dockerfile` (see
  [experiment 04](04-dockerfile-parser.md)) and the other crates in survey
  chapter 03 §3 were not tested here.
- Sparse files, files > 8 GiB, cgroup v1 and `--cpuset-cpus` under a
  rootful runtime were not tested. [Experiment 16](16-cgroup-v1-cpuset.md)
  later examined CPU detection by other runtimes under cgroup v1 and cpuset.

## Reproduction

The PoC lived in `docs-rust/poc/05-rust-ecosystem/` (archived outside this
repository). No Rust is needed on the host: every build runs in a container.

| Script | Purpose |
|---|---|
| `scripts/q1-build.sh`, `scripts/q1-run.sh` | Static binary; CA handling in scratch and busybox |
| `scripts/q1-aarch64.sh` | aarch64 cross build, run under qemu-user |
| `scripts/mkdata.sh` | Create `out/data.bin` (needed by Q2 and Q3) |
| `scripts/q2-build.sh`, `scripts/q2-run.sh` | Allocator benchmark |
| `scripts/build-all.sh` | Build `pargz`, `ocipoc`, `tarpoc`, `nproc` |
| `scripts/q3-run.sh` | Parallel compression |
| `scripts/q4-run.sh` | `oci-client` against `registry:2` on `127.0.0.1:5055`, and `docker_credential` |
| `scripts/q5-run.sh` | tar |
| `scripts/q6-run.sh` | `available_parallelism()` |

- `scripts/c.sh <musl|gnu> -- <cmd>` runs a command in `rust:1-alpine` or
  `rust:1-bookworm`, with the PoC mounted at `/work` and a `rk-cargo` volume
  for the cargo registry (`scripts/env.sh` holds the shared settings).
- The build directory is `target/` (about 1.5 GB, deleted after the run).
  Binaries and `data.bin` go to `out/` and are not kept; only `out/*.txt`
  is.

## References

1. [Survey chapter 03: Rust architecture](../survey/03-rust-architecture.md), §1, §3 and the code-size estimates.
2. [Survey chapter 05: Decisions](../survey/05-decisions.md), §1 (aarch64,
   Q7) and §2 (`--jobs`, Q11).
3. `oci-client` 0.18.0, `src/client.rs`
   (<https://github.com/oras-project/rust-oci-client>). **[code]**
4. `gzp` 2.0.4, `check.rs`. **[code]**
5. crates.io and GitHub metadata for `docker_credential`, `oci-spec` and
   `oci-client` (accessed 2026-10-29). **[external]**
