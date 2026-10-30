---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0036: Kaniko flags in Phase 1: accepted, ignored with a warning, or refused with the phase

## Context and Problem Statement

`ADR-0011` keeps kaniko's flag names and meanings and refuses, with a reason,
a kaniko flag that ranikust does not implement; `ADR-0023` adds one exception:
`--snapshot-mode` is accepted and ignored with a warning in overlay mode. Until
now every other kaniko flag outside a short list was refused "until the flag
list is decided" ([Q-023](../open-questions.md#q-023-which-other-kaniko-flags-and-variables-are-accepted)),
together with `--custom-platform` (Q-020), `--cache` (Q-021) and the logging
flags (Q-031). Pipelines that swap kaniko's executor for ranikust pass many of
these flags, and the Phase 1 acceptance needs the list fixed, each accepted
flag tested for its meaning and the mapping in the book. kaniko also reads
environment variables (`KANIKO_*`, 68 `FF_KANIKO_*` feature flags) that
ranikust ignored silently.

## Considered Options

- Refuse every kaniko flag ranikust does not implement (`ADR-0011` as
  written): safe, but breaks pipelines over flags that change nothing in
  overlay mode.
- Accept and ignore every unknown kaniko flag: keeps pipelines running, but a
  flag that would change the image is then silently dropped.
- Four classes, chosen per flag by whether ignoring it changes the image
  (taken): implement it, accept it and warn, refuse it until a phase, or
  refuse it for good.

## Decision Outcome

Every kaniko executor flag and variable falls into one of four classes, and a
flag whose absence would change the image (contents, config, manifest or
format) is never ignored: it is implemented or refused with the phase that
brings it.

1. **Accepted, kaniko's meaning** (tested for it):
   - Logging: `-v`/`--verbosity` (`fatal` and `panic` log as `error`),
     `--log-format=text|color|json` (`color` only when stderr is a terminal,
     as logrus), `--log-timestamp`, **off by default as in kaniko**. The
     level filters logs only, never the metrics report (`ADR-0025`).
     Per-stage grouping stays apart from the format (Q-031 option (a)).
   - Image: `--label` (on the pushed stage only, set before its first
     instruction, so a `LABEL` of the same key wins), `--annotation`
     (manifest annotations; the base image's are not carried over),
     `--image-format=docker|oci` (default OCI, `ADR-0032`; `docker` with
     `--compression=zstd` exits 2 as in kaniko), `--compression-level`.
   - Outputs: `--digest-file`, `--image-name-with-digest-file` (needs a
     destination), `--image-name-tag-with-digest-file`, in kaniko's formats.
   - Registries: `--insecure` (destinations), `--insecure-pull` (registries
     of `FROM` and `COPY --from` images), `--skip-tls-verify`,
     `--skip-tls-verify-pull`, `--skip-tls-verify-registry`. ranikust's
     client keeps per-registry sets, so a registry pushed to and pulled from
     gets the union of both directions.
   - Retries: `--push-retry` and `--image-download-retry` replace ranikust's
     default policy for their operations **only when given**, with kaniko's
     backoff (N retries, waiting 1 s, then twice as long each time);
     `--push-retry=0` means no retry.
   - Flags whose meaning holds without code: `--skip-push-permission-check`
     (ranikust makes no write check before the build) and
     `--preserve-context` (overlay mode never touches the context).
   - `--custom-platform` is an alias of `--platform` (Q-020), so only
     `linux/amd64` until `ADR-0009`'s targets land.
   - `--cache=false` is accepted: it asks for exactly what Phase 1 does
     (Q-021).
   - kaniko's deprecated spellings `--tarPath`, `--snapshotMode` and
     `--customPlatform` are aliases, with kaniko's deprecation warning.
2. **Accepted and ignored with one warning** (extends `ADR-0023`'s rule):
   `--force`, `--cache-dir`, `--use-new-run`, `--cleanup`, `--pre-cleanup`,
   `--image-fs-extract-retry`, `--context-sub-path` (it applies only to
   fetched contexts, Q-081), `--force-build-metadata`,
   `--skip-unused-stages`. Kaniko mode (Phase 2) implements or refuses the
   kaniko-mode ones there.
3. **Refused until a phase:** the cache flags (`--cache`, `--cache=true`,
   `--cache-repo`, `--cache-ttl`, `--cache-copy-layers`, `--cache-run-layers`,
   `--compressed-caching`, `--no-push-cache`; Phase 4), `--ignore-path`,
   `--single-snapshot`, `--materialize`, `--ignore-var-run` (with Q-035) and
   its old name `--whitelist-var-run` (Phase 2), `--git`, the mirror, map and
   credential-helper flags, `--skip-default-registry-fallback`,
   `--registry-client-cert`, `--push-ignore-immutable-tag-errors` (Phase 5),
   and `--dryrun` (not scheduled; a stage-plan output of ranikust's own, not
   kaniko's format).
4. **Refused for good:** `--bucket`, `--azure-container-registry-config` and
   the `bake` subcommand (`ADR-0008`); `--kaniko-dir` stays refused naming
   `--state-dir`, since the layouts differ.

Values:

- `--compression-level`: gzip `-1` (Go's default) is 6, gzip 0 to 9 and zstd
  1 to 19 are used as given; gzip `-2` (Huffman only) and zstd 20 to 22,
  which kaniko accepts, are refused with a reason; without the flag,
  ranikust's default levels stay (the level is part of the layer bytes).
- `--image-format=oci` with `--tar-path` is allowed, unlike kaniko: the
  tarball then holds OCI media types, which `docker load` and `podman load`
  read.

Environment variables:

- `KANIKO_NO_PUSH` overrides `--no-push`, as kaniko does.
- `KANIKO_REGISTRY_MIRROR` and `KANIKO_REGISTRY_MAP` exit 2: silently pulling
  from another registry than a mandated mirror would be a policy breach.
- `DOCKER_AUTH_CONFIG` is read before `config.json`, as kaniko's keychain
  order.
- `KANIKO_PRESERVE_CONTEXT` is honoured as its flag; `KANIKO_DIR` gets a
  warning naming `--state-dir`.
- Every other set `KANIKO_*` or `FF_KANIKO_*` variable is named in one
  start-up warning, as kaniko warns about unknown feature flags. In
  particular there is **no `KANIKO_METRICS_FILE` alias**: the report path is
  `--metrics-file` or `RANIKUST_METRICS_FILE` (`ADR-0025`).

### Consequences

- Good, because a pipeline that swaps kaniko's executor keeps its flags, and
  no flag that changes the image is dropped silently.
- Good, because every set kaniko variable is either honoured, refused or
  named in a warning.
- Bad, because the ignored class must be revisited in Phase 2, where some of
  those flags (`--cleanup`, `--use-new-run`) gain a meaning in kaniko mode.
- Bad, because `--log-timestamp` off by default changes ranikust's earlier
  log lines, which had timestamps.
- Neutral, because the insecure and TLS flags apply per registry, not per
  direction, when one registry is both pushed to and pulled from.

## Evidence

- kaniko's flags: `kaniko@44aa7ddc0 cmd/executor/cmd/root.go:60-68` (logging,
  `--force`), `:332-388` (`addKanikoOptionsFlags`), `:392-409`
  (`AddRegistryOptionsFlags`), `:413` (hidden ACR flag); validations
  `:178-187` (image format and zstd), `:212-214` (image-name digest file
  needs a destination), `:499-508` (compression levels); variables `:75-92`
  (`KANIKO_REGISTRY_MIRROR`, `KANIKO_NO_PUSH`, `KANIKO_REGISTRY_MAP`);
  deprecated spellings `:456-466` [code].
- Labels on the pushed stage at stage init: `pkg/executor/build.go:190-191`,
  `:275-285`; annotations `:2128` [code].
- Digest files: `pkg/executor/push.go:194-256` (one text, tagged when the tag
  file is asked for, written to both files) [code].
- Retries: `pkg/executor/push.go:364`, `pkg/image/remote/remote.go:79,121`,
  `pkg/util/util.go:221` (`Retry`: 1 s initial delay, doubling) [code].
- `DOCKER_AUTH_CONFIG` before the file: `pkg/creds/creds.go:39-49` [code].
- Feature flags: `pkg/config/featureflags.go:131-198` and kaniko's README
  sections, checked flag by flag for the book page [code].
- Tests: `ranikust` `cli::tests` (refusals, ignored flags, aliases, logging
  values), `config::tests` (levels, formats, labels, digest-file rules,
  variables), `build::tests` (per-registry sets, digest file formats),
  `tests/cli.rs` `logging_flags_change_what_is_written` and
  `a_registry_mirror_variable_is_refused`; `rnk-builder`
  `labels_go_to_the_target_stage_and_a_label_instruction_wins` and the
  `--image-format`/`--annotation` assertions of
  `assembly_converts_media_types_and_is_stable`; `rnk-registry`
  `push_and_pull_retries_are_set_apart`,
  `docker_auth_config_comes_before_the_file`; `rnk-metrics`
  `the_log_level_is_the_log_layers_hint_in_every_format`.

## More Information

- Extends `ADR-0011` (a fourth class, accept-and-warn) and `ADR-0023`'s rule
  for `--snapshot-mode`; does not change them.
- Answers Q-020, Q-021, Q-023 and Q-031; the flag table is in
  [kaniko-map.md](../agents/kaniko-map.md#flags) and the user-facing mapping
  in the book's usage page.
- Revisit the ignored class in Phase 2 (kaniko mode), the cache flags in
  Phase 4, and the registry flags in Phase 5.
