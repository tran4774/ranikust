---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0030: Phase 1 dependencies: rustix, sha2, gzip by hand, reqwest, own config.json reader, clap

## Context and Problem Statement

Survey 03 §3 left six dependency picks open for Phase 1 (O5 of
[phase-1](../agents/phase-1.md#open-questions-blocking-phase-1),
[Q-012](../open-questions.md#q-012-which-crates-fill-the-phase-1-dependency-picks)):
the syscall crate, the sha256 implementation, parallel gzip, the HTTP
stack, Docker credentials and the CLI parser. Tasks 4, 5, 6 and 8 had to
start before the question was answered, so each took an interim pick,
reversible because none of these crates appears in a library's public API.
The question is whether those interim picks become the decision.

## Considered Options

- Syscalls: `rustix` or `nix`.
- sha256: `sha2` or `aws-lc-rs`; `ring::digest`, which arrived later as the
  TLS provider (`ADR-0024`), was raised in review.
- Parallel gzip: `gzp` or a pigz-style compressor written in `rnk-layer`.
- HTTP: `reqwest` or `hyper` directly.
- Credentials: `docker_credential` as is, vendored, or ranikust's own
  `config.json` reader.
- CLI parser: `clap` or another.

## Decision Outcome

Phase 1 keeps the picks the code already made: `rustix`, `sha2`, gzip by
hand, `reqwest`, its own `config.json` reader and `clap`.

- **Syscalls:** `rustix`, with `libc` only for calls `rustix` lacks
  (`clone`, `wait4`, `execve`, capability `prctl`s, `getrusage`). Not `nix`.
- **sha256:** `sha2`, behind `rnk_layer::Sha256Digest`. Not `aws-lc-rs`.
  `ring::digest` against `sha2` on large layers is still to be measured
  ([Q-033](../open-questions.md#q-033-which-performance-conditions-are-still-unmeasured));
  a switch stays inside `Sha256Digest`.
- **gzip:** the pigz-style compressor in `rnk-layer` (`gzip.rs`, `flate2`
  on zlib-rs). Not `gzp`.
- **HTTP:** `reqwest` with `rustls-no-provider`, the stack `oci-client`
  builds on. `hyper` only in tests (the fake registry).
- **Credentials:** `rnk_registry::Credentials` reads `config.json` itself.
  Credential helpers (`credHelpers`, `credsStore`) are recognised but not
  run until Phase 5 ([Q-047](../open-questions.md#q-047-what-are-the-mirror-registry-map-and-helper-flags-called));
  `docker_credential` stays a candidate for them.
- **CLI:** `clap` 4 with derive and no colour; its types stay in the
  `ranikust` binary.

### Consequences

- Good, because the build stays pure Rust apart from what zig already
  compiles (libzstd, mimalloc, ring): nothing needs CMake or a C toolchain
  beyond `ADR-0020`'s.
- Good, because the compressed bytes and digests do not depend on the
  thread count without any special configuration.
- Good, because one HTTP stack serves both `oci-client` and ranikust's own
  requests.
- Bad, because ranikust maintains its own gzip framing (`gzip.rs`, about 250 lines)
  and its own `config.json` reader (`credentials.rs`, about 390 lines with its tests).
- Bad, because `ring` and `sha2` both carry a sha256 until Q-033 is
  measured.

## Evidence

- [Experiment 05](../research/experiments/05-rust-ecosystem.md), gzip
  table: the hand-written pigz-style compressor gives identical bytes at
  every thread count, 2216–2234 MB/s at t = 32 and 367,282,454 bytes.
  `gzp` without a dictionary changes its output between t = 1 and t ≥ 2
  unless forced to `ParCompress`, reaches 1985–2052 MB/s at t = 32, and
  writes 372,927,311 bytes. `gzp` with a dictionary is not deterministic.
- Experiment 05, summary: a static-pie musl binary with tokio, reqwest
  (rustls), zstd, flate2 and sha2 is 5.1 MB with `ring` and 7.3 MB with
  `aws-lc`.
- [Survey 03](../research/survey/03-rust-architecture.md) §3: the original
  candidates and criteria; `docker_credential` 1.4 is "usable, small, few
  maintainers".
- `Cargo.lock` holds no `nix`, `aws-lc-rs`, `gzp` or `docker_credential`
  [code].
- `sha2` 0.11 detects SHA-NI and the ARMv8 SHA extensions at run time
  [external].

## More Information

- Answers [Q-012](../open-questions.md#q-012-which-crates-fill-the-phase-1-dependency-picks)
  and resolves O5 of Phase 1. Phase 2 picks stay open in
  [Q-038](../open-questions.md#q-038-which-crates-fill-the-phase-2-dependency-picks).
- Related: `ADR-0020` (static musl via cargo-zigbuild), `ADR-0024`
  (rustls with `ring`), `ADR-0029` (zstd frames). The crate list is in
  [refs.md](../agents/refs.md#crates).
- Revisit sha256 when Q-033 measures `ring::digest`, and credentials when
  Phase 5 runs credential helpers.
