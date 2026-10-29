---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0026: `rnk-oci` owns its OCI and Docker image types instead of using `oci-spec`

## Context and Problem Statement

The crate card and `ARCHITECTURE.md` planned `rnk-oci` as a thin layer over
the `oci-spec` crate (0.10). Implementing Phase 1 task 1 showed that
`oci-spec`'s image types cannot meet two invariants: image config and
manifest bytes must be deterministic, and a base image's config must be
carried over without losing fields. Should `rnk-oci` wrap `oci-spec`, or
define its own types?

## Considered Options

- Own types in `rnk-oci`, serialized as canonical JSON.
- Use `oci-spec` and fix its problems upstream or in a fork.
- Own types for the image config only, `oci-spec` for manifests and indexes.

The second option leaves determinism to an upstream release schedule; the
third keeps two type families for one image and still pulls `oci-spec`'s
dependencies into every crate that needs a `Digest`.

## Decision Outcome

`rnk-oci` defines its own OCI and Docker v2s2 types (digests, media types,
descriptors, platforms, manifests, indexes, image configs, references) and
writes them as canonical JSON with sorted keys; it does not depend on
`oci-spec`.

- `to_canonical_json` sorts the keys of every object, including struct
  fields, at write time, emits compact output and rejects floats.
- The config type carries the same field set as go-containerregistry, which
  kaniko carries over from a base image, including `Healthcheck`, `OnBuild`,
  `Shell` and `ArgsEscaped`.
- `rnk-registry` still uses `oci-client`, which depends on `oci-spec`; it
  exchanges raw manifest bytes and converts references through their string
  form, so no `oci-spec` type crosses a crate boundary.

### Consequences

- Good, because config and manifest bytes are identical across runs and
  insertion orders, which digests and `--reproducible` need.
- Good, because no field of a base image's config is silently dropped.
- Good, because crates that only need `Digest` (`rnk-layer`, `rnk-store`)
  do not pull `oci-spec`'s builder and regex dependencies.
- Bad, because ranikust maintains its own types and must follow new
  image-spec and distribution-spec releases itself.
- Neutral, because canonical output differs from Go's encoder in escaping
  (`<`, `>`, `&` are written raw), so such configs differ from kaniko's byte
  for byte; recorded on the crate card.

## Evidence

- [code] `oci-spec` 0.10.0: labels, annotations, `ExposedPorts` and
  `Volumes` are `HashMap`s, and `ExposedPorts`/`Volumes` are read into a
  `Vec` in hash order, so two parses of one config can serialize
  differently.
- [code] `oci-spec` 0.10.0 `Config` has no `Healthcheck`, `OnBuild`,
  `Shell` or `ArgsEscaped`; `Platform` writes `os_version` instead of
  `os.version`.
- [measured] `crates/rnk-oci` tests: canonical bytes are identical over 100
  runs of HashMap-ordered input and under random insertion orders
  (property tests), and seven manifest, index and config fixtures round-trip
  to the same JSON value.

## More Information

- Refines `ARCHITECTURE.md` and the `rnk-oci` card, which planned
  `oci-spec`; both now describe the owned types.
- Related: [ADR-0024](0024-tls-rustls-ring.md) (`oci-client` stays in
  `rnk-registry`), [ADR-0012](0012-accept-divergence-between-modes.md)
  (determinism).
- Implemented in branch `feat/rnk-oci-types` (merged 2026-10-30).
- Revisit if `oci-spec` gains ordered maps and the missing config fields.
