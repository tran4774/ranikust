---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0020: Static musl binary; cargo-zigbuild for cross builds

## Context and Problem Statement

kaniko ships as an image containing the executor, `tini` and several
`docker-credential-*` helpers. ranikust must be one file that runs on any
Linux host and inside an empty (`FROM scratch`) image, with no dependency on
the host's glibc, OpenSSL or CA bundle. Static musl binaries have known traps:
musl's allocator is slow under threads, and TLS stacks often expect system
certificates. How is the binary built and linked?

## Considered Options

- Dynamically linked glibc binary. Depends on the host's glibc.
- Static glibc. Fragile (NSS, `dlopen`), not a supported Rust target.
- Static musl with musl's default allocator.
- Static musl with mimalloc, rustls with explicit CA handling, and
  cargo-zigbuild for cross-compiling.

## Decision Outcome

ranikust is released as one statically linked static-pie binary for
`x86_64-unknown-linux-musl`, with mimalloc as the global allocator, rustls
with an explicitly built trust store, and cargo-zigbuild for cross builds.

- **musl static-pie.** No dynamic loader and no `NEEDED` entries; CI checks
  both. No OpenSSL anywhere in the dependency tree.
- **mimalloc is required**, not optional: musl's allocator collapses under
  threads, and snapshotting is many small allocations on many threads.
- **rustls trust store.** Build the `rustls::ClientConfig` explicitly: the
  host's CAs (`/etc/ssl/certs`, `SSL_CERT_FILE`, `SSL_CERT_DIR`) if any, else
  the embedded webpki roots; `--registry-certificate` adds CAs as in kaniko.
  `oci-client` creates its own HTTP client and accepts only certificate
  lists, so it receives DER certificates (`webpki-root-certs`) rather than a
  prepared config.
- **Cross builds** (`aarch64-unknown-linux-musl`, [ADR-0009](0009-x86-64-first-aarch64-ready.md))
  use cargo-zigbuild with zig; no separate musl cross toolchain.
- Release profile: fat LTO, one codegen unit, `panic = "abort"`, debug info
  stripped but symbols kept (`Cargo.toml`).
- The rustls crypto provider (`ring` or `aws-lc-rs`) is chosen at
  implementation time; `ring` gives the smaller binary.

### Consequences

- Good, because the same file runs on any host and in an empty image, and
  replaces kaniko's executor, `tini` and bundled credential helpers.
- Good, because multi-threaded allocation performance matches or beats glibc.
- Bad, because credential helpers stay external binaries: used when present
  on the host, not shipped.
- Bad, because C dependencies (zstd, the crypto provider) must keep
  cross-compiling through zig.

## Evidence

- [Experiment 05](../research/experiments/05-rust-ecosystem.md): tokio,
  reqwest with rustls, zstd, flate2 (`zlib-rs`) and sha2 built as a
  5.1 MB static-pie with `ring` (7.3 MB with `aws-lc-rs`) that ran in an
  empty rootfs. Sixteen threads of small allocations: musl 2.8 Mops/s, musl
  with mimalloc 663–753, glibc 793–1004 (about 250–300× between musl and
  mimalloc); mimalloc adds about 150 KB. reqwest 0.13 dropped its
  `webpki-roots` feature, and its default verifier failed in an empty rootfs
  with `No CA certificates were loaded from the system`; the explicit
  webpki-plus-host config worked everywhere. cargo-zigbuild 0.24.0 with zig
  0.15.2 produced static aarch64 binaries that ran under
  `qemu-aarch64-static`.
- [Survey 03](../research/survey/03-rust-architecture.md) §1.

## More Information

Passing the DER bundle to `oci-client` through `tls_certs_only` was not tested
in the experiment. jemalloc was not measured.

The trust store combines host CAs and embedded roots, with the `ring`
provider ([ADR-0024](0024-tls-rustls-ring.md)).
