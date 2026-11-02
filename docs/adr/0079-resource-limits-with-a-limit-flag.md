---
status: accepted
date: 2026-11-02
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0079: Registry and layer inputs are bounded by default, and `--limit NAME=VALUE` changes the bounds

## Context and Problem Statement

Threat model T15 promises that a hostile input fails cleanly instead of
exhausting the machine: "stream instead of buffering; fail cleanly on
exhaustion". Four places did not keep it, each reachable from a registry or
image a Dockerfile names (`FROM`, `COPY --from`):

- the `tar` crate reads every extension header body (GNU long name `L`,
  GNU long link `K`, PAX records `x`) whole into memory, at whatever size
  its header declares, so a small compressed layer could make ranikust
  allocate gigabytes; Q-056 had answered "no bound for now";
- `oci-client` collects manifest, token-service and error bodies whole with
  `Response::bytes()` before ranikust's 4 MiB manifest check applied;
- an image config was pulled into memory up to the size its manifest
  declares, with no cap;
- registry requests had no read timeout: a registry that keeps the
  connection open and sends nothing held a pull forever, and with it the
  store's cross-process per-digest lock that other builds on the same
  state directory wait for.

The maintainer decided on 2026-11-02 to bound all four by default, with the
values proposed in the hardening review, and to let operators change them
with one flag, `--limit NAME=VALUE`, rather than one flag per bound.

## Considered Options

- Fixed limits, no flag.
- One flag per limit (`--max-manifest-size`, `--registry-idle-timeout`, ...).
- One repeatable flag, `--limit NAME=VALUE`, over a table of named limits.

Fixed limits leave a legitimate but unusual image (a 20 MiB config) with no
way through. One flag per limit adds a flag for every later bound (entry
count, path depth, decompressed layer size). One repeatable flag keeps the
command line small and takes later limits as new names, as Docker's
`--ulimit nofile=...` and BuildKit's `--opt k=v` do.

## Decision Outcome

ranikust bounds what a registry or an image can make it hold or wait for,
by default, and `--limit NAME=VALUE` (repeatable, the last of one name
wins) changes a bound.

| Name | Default | Bounds |
|---|---|---|
| `tar-header` | `1MiB` | each GNU long name, GNU long link or PAX record set of a layer ranikust extracts |
| `manifest` | `4MiB` | a manifest or index body, refused while it arrives |
| `registry-body` | `1MiB` | a token response; an error body is read no further than this |
| `config` | `8MiB` | an image config, checked against its descriptor before the request |
| `registry-idle` | `2m` | how long a registry may send nothing before the request fails and is retried |

- Sizes are a number of bytes, or a number with `KiB`, `MiB` or `GiB`;
  durations a number with `s`, `m` or `h`. A value must be above 0 and at
  most 1 GiB (sizes) or 24 h (durations), and there is no `unlimited`: an
  operator who needs more names a larger number within the ceiling. An
  unknown name or a bad value is refused at start-up with every name, its
  default and what it bounds.
- A limit hit fails the build with an error that names the limit and how
  to raise it, for example `manifest ... is larger than 4 MiB (limit
  "manifest"); raise it with --limit manifest=<size>`. It is not retried.
- `registry-idle` bounds silence, not the whole transfer: a large layer on
  a slow link keeps going as long as bytes arrive. There is no overall
  deadline.
- The limits in force are logged at debug level and recorded in the
  metrics report (`options.limits`: sizes in bytes, durations in
  milliseconds).
- The names and defaults live in one table in the CLI crate; each library
  keeps its own default as a constant (`rnk_rootfs::
  DEFAULT_EXTENSION_HEADER_LIMIT`, `rnk_registry::ClientOptions::
  MAX_MANIFEST_BYTES` and the others), so a library used without the CLI
  is bounded too.

How each bound is kept:

- **`tar-header`.** A pass-through reader between the decompressed layer
  and the `tar` crate follows the archive's 512-byte block framing exactly
  as the `tar` crate does, including the `size` a PAX record set gives the
  next entry, and fails the read that completes the header of an
  extension larger than the limit, before the `tar` crate reads its body.
  It covers every extraction path, since all of them go through
  `rnk_rootfs`'s one extractor. The limit is `rnk-rootfs`'s only
  process-wide setting, set once by the binary before any build.
- **`manifest`, `registry-body`.** `rnk-registry` now sends manifest and
  blob `GET`s and anonymous or basic token requests itself, reading bodies
  in chunks with the limit checked as they arrive (and against
  `Content-Length` first), instead of through `oci-client`. Wrapping
  `oci-client` was not possible: it builds its own `reqwest` client and
  collects bodies inside its methods. Status handling and digest checks
  follow `oci-client`'s, so errors read as before. `oci-client` keeps the
  push side (blob existence, uploads, mounts, manifest pushes).
- **`config`.** `Client::pull_config` refuses a descriptor larger than the
  limit before any request; the existing size bound then holds the read to
  the descriptor's size.
- **`registry-idle`.** The read timeout of every `reqwest` client
  ranikust builds, and of `oci-client`'s. A stalled pull fails, is retried
  under the retry policy, and ends with an error; the store's fill then
  releases the per-digest lock through its error path.

This overrides Q-056's answer ("no bound for now; revisit with `cargo fuzz`
in Phase 6") for extension headers. Fuzzing the extractor stays a Phase 6
task.

### Consequences

- Good, because T15 holds for these inputs: memory stays near the limit
  whatever a registry sends or a layer declares, measured by counting
  allocators in the tests.
- Good, because a registry can no longer hold the store's lock by sending
  nothing, so it cannot block other builds on a shared state directory
  indefinitely.
- Good, because ranikust's bound on extension headers matches what kaniko
  and BuildKit read layers with (Go's `archive/tar`, 1 MiB).
- Bad, because an image that is valid but unusual (a config over 8 MiB, a
  PAX record set over 1 MiB) now needs a `--limit` to build.
- Bad, because `rnk-registry` carries more of the distribution protocol
  itself (manifest and blob pulls, token requests).
- Neutral, because two gaps remain, each logged: a registry that sends one
  byte per interval still holds a pull, and its lock, as long as it keeps
  sending [open: Q-448]; and on the push side `oci-client` still collects
  error bodies whole, from registries the operator named as destinations
  [open: Q-449].

## Evidence

- `tar-0.4.46 src/archive.rs:409-440`: `L`, `K` and `x` bodies go through
  `EntryFields::read_all`, sized by the header **[code]**.
- `tar-0.4.46 src/archive.rs:285-385`: the framing the reader follows,
  including the PAX `size` override **[code]**.
- `oci-client-0.18.0 src/client.rs:905,913,1176,1363`: `text()`/`bytes()`
  on token, manifest and blob error bodies **[code]**.
- `crates/rnk-layer/src/header.rs`: `MAX_PAX_SIZE`, Go's
  `maxSpecialFileSize` of 1 MiB, already bounds what ranikust writes
  **[code]**.
- Tests (T0): `rnk-rootfs` `tests/limits.rs`
  `extension_headers_are_bounded_by_the_tar_header_limit` (with a control
  that holds the whole header without the bound) and the framing unit
  tests in `src/extract/limit.rs`; `rnk-registry` `tests/limits/main.rs`
  `limits_refuse_hostile_bodies_with_bounded_memory` (64 MiB hostile bodies
  held under 16 MiB); `rnk-builder` `tests/stall.rs`
  `a_stalled_blob_pull_releases_the_store_lock`; `ranikust`
  `limits_repeat_and_bad_ones_are_refused_at_start_up` **[measured]**.

## More Information

- Threat model T15; Q-056 (answered 2026-10-30, overridden here); Q-448,
  Q-449.
- ADR-0070: references still reach `oci-client` built from their parts,
  now only for the push side; manifest and blob pulls no longer go
  through it.
- [rootfs design](../design/rootfs.md), [metrics design](../design/metrics.md),
  the book's usage page.
