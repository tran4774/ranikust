---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0120: Credential helpers are external for now, mirrors keep kaniko's flags, and pushes go through rnk-registry

## Context and Problem Statement

Three parts of Phase 5's registry work were undecided (see [the
roadmap](../../README.md#roadmap)):

- **Credential helpers.** [ADR-0020](0020-static-musl-via-cargo-zigbuild.md)
  keeps helpers as external binaries, used when present.
  [ADR-0078](0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md)
  reads credentials once, before the first instruction, and
  [ADR-0101](0101-a-build-runs-from-a-sealed-copy-of-the-binary.md) runs
  ranikust from a sealed copy, because in kaniko mode on `/` a `RUN` can replace
  any file there. A `docker-credential-*` helper, an external `git` or `ssh` has
  no such protection, while helper tokens are short-lived (BuildKit asks again
  after 4m50s). Two questions follow: which host binaries may ranikust run
  during a build, and when do helpers run; and are the cloud helpers built in?
  kaniko links in five (`env`, `google`, `ecr`, `acr`, `gitlab`), all on by
  default and chosen with `--credential-helpers`.
- **Mirrors.**
  [ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md) decided
  that a mirror never gets another registry's credentials nor a push, and that
  flags keep kaniko's names only with kaniko's meaning. Which pulls do mirrors
  serve, and with which flags? kaniko's `--registry-mirror` mirrors Docker Hub
  only, `--registry-map` any registry, and mirrors serve `FROM` and
  `COPY --from`, never a push or the cache. BuildKit mirrors any registry,
  always falls back, and also serves `RUN --mount from=` and cache imports.
- **Pushes.** [#27](https://github.com/tran4774/ranikust/issues/27) (Q-449):
  pushes still go through `oci-client`, which reads error bodies whole and has
  no idle bound on some requests. `oci-client` 0.18 builds its own `reqwest`
  client from a configuration with no client-certificate field, so kaniko's
  `--registry-client-cert` cannot reach a push destination through it.

## Considered Options

When helpers run, and which host binaries:

- (a) Everywhere, each helper runs once before the first instruction, for every
  registry the build will contact, and its tokens are kept for the build: a long
  build can fail at push on an expired token.
- (b) On `/` as (a), and the external `git` and `ssh` refused there once a `RUN`
  has run; elsewhere a helper runs on first need per host and again after 4m50s,
  as BuildKit.
- (c) On `/`, refuse credential helpers and the external `git` altogether.

Built-in cloud helpers:

- (a) External only: `--credential-helpers` refused for good, with its reason.
- (b) Link in `google`, `ecr` and `acr` behind `--credential-helpers`: kaniko
  pipelines work unchanged, at the price of a larger static binary with
  cloud-SDK dependencies to audit.
- (c) (a) now, (b) in a later phase.

Mirrors:

- (a) kaniko's three flags and meaning; mirrors serve every image pull of the
  build, never pushes nor the cache repository.
- (b) (a), and cache imports go through mirrors too, as BuildKit.
- (c) A BuildKit-style configuration file instead of flags.

Pushes (#27):

- (a) Leave pushes in `oci-client`: `--registry-client-cert` then works for
  pulls only, which is not kaniko's meaning, so ADR-0049 would refuse the flag.
- (b) Send push requests from `rnk-registry` too, as
  [ADR-0079](0079-resource-limits-with-a-limit-flag.md) did for pulls.

## Decision Outcome

Credential helpers are external `docker-credential-*` binaries for now, run once
before the first instruction on `/` and on first need per host elsewhere;
registry mirrors keep kaniko's three flags and meaning and serve every image
pull; and push requests are sent by `rnk-registry` instead of `oci-client`.

The maintainer's answer (2026-11-06: "tạm thời làm theo ý 1, nhưng tương lai sẽ
phải gắn sdk của các cloud provider", "for now do option 1, but in the future we
will have to integrate the cloud providers' SDKs") stood for option (b) of when
helpers run, option (c) of the built-in helpers, option (a) of the mirrors and
option (b) of #27.

- **When helpers run (b).** On `/`, each helper runs once before the first
  instruction for every registry the build will contact, and the tokens are kept
  for the build; the external `git` and `ssh` are refused there once a `RUN` has
  run, so nothing a `RUN` could have replaced is ever run (ADR-0078's rule).
  Elsewhere a helper runs on first need per host and again after 4m50s, as
  BuildKit. A helper inherits ranikust's environment, as docker/cli and BuildKit
  run it (cloud helpers read it), bounded by `fetch-idle`
  ([ADR-0118](0118-fetches-are-bounded-by-fetch-idle-and-http-contexts-and-fetches-under-network-none-are-refused.md)).
- **External helpers only, for now (c).** `--credential-helpers` is refused with
  a "not yet" reason, not "for good": the maintainer expects to integrate the
  cloud providers' SDKs, so built-in `google`, `ecr` and `acr` helpers behind
  the flag are planned for a later phase, tracked in the Backlog as
  [#138](https://github.com/tran4774/ranikust/issues/138). Until then the book
  shows the `credHelpers` entry of `config.json` and the binaries to install.
- **Mirrors (a).** `--registry-mirror` (Docker Hub), `--registry-map` (any
  registry) and `--skip-default-registry-fallback` keep kaniko's names and
  meaning. Mirrors serve every image pull of the build: `FROM`, `COPY --from`,
  `RUN --mount from=` and the images `ONBUILD` adds. They never serve a push or
  the cache repository, and never receive another registry's credentials
  (ADR-0049).
- **Pushes (#27 (b)).** Blob existence checks, uploads, mounts and manifest
  pushes are sent by `rnk-registry`: client certificates reach push
  destinations, so `--registry-client-cert` keeps kaniko's meaning; the
  `registry-body` limit covers their error bodies; there is one HTTP stack.

### Consequences

- Good, because on `/` nothing a `RUN` could have replaced is ever run.
- Good, because ADR-0020 stands for now: the binary carries no cloud SDK to
  audit.
- Good, because mirrors keep kaniko's meaning, and an image a mount or a trigger
  names is the same kind of pull as `FROM`.
- Good, because client certificates work on pushes, push error bodies are
  bounded and one HTTP stack remains.
- Bad, because on `/` a long build can fail at push on an expired helper token.
- Bad, because a kaniko pipeline that relies on built-in cloud helpers needs a
  `config.json` entry and the helper binary until the built-ins land;
  `--credential-helpers` fails meanwhile.
- Bad, because `rnk-registry` grows by the push path, which must be tested
  against the registries `oci-client` served **[inferred]**.
- Neutral, because the later built-ins will link cloud SDKs into the static
  binary, a larger binary with dependencies to audit, which the maintainer
  accepts for later.

## Evidence

- **[external]** kaniko@44aa7ddc0: `--credential-helpers` takes
  `env, google, ecr, acr, gitlab`, all on by default, each Go code linked in
  (`cmd/executor/cmd/root.go:409`, `pkg/creds/creds.go:50-91`); its image also
  ships `docker-credential-gcr`, `-ecr-login` and `-acr-env`
  (`deploy/Dockerfile:73-80`). `--registry-mirror` is Docker Hub only
  (`cmd/executor/cmd/root.go:99-104`); mirrors apply to `FROM` and
  `COPY --from=<image>`, never to a push or the cache
  (`pkg/image/image_util.go:107`, `pkg/executor/build.go:2311`,
  `pkg/executor/push.go:276-299`, `pkg/cache/cache.go:58-94`).
  `--registry-client-cert` is per registry transport
  (`pkg/util/transport_util.go:143-185`).
- **[external]** buildkit@cd8ee244: the client caches credentials per host for
  4m50s (`session/auth/authprovider/authprovider.go:60-66`); the helper is
  `docker-credential-<suffix> get`, the environment inherited
  (docker-credential-helpers `client/command.go:33-42`); mirrors serve every
  image pull, `RUN --mount from=` and cache import included
  (`util/resolver/resolver.go:124-163`, `source/containerimage/pull.go:94`,
  `:207`).
- **[code]** Helpers named in `config.json` are parsed but never run: the client
  warns and goes anonymous (`crates/rnk-registry/src/client.rs:1176-1184`).
  `oci-client` 0.18's client configuration has no client-certificate field
  (`client.rs:330-375` in the crate).

## More Information

Answers #27 and three Phase 5 questions (when helpers run, the built-in helpers,
mirrors). ADR-0020 and ADR-0049 stand; ADR-0079 is extended to pushes, which it
left open as #27, and is not rewritten. Related:
[ADR-0036](0036-kaniko-flags-in-phase-1.md) (the flags refused until Phase 5).

Not settled here:

- When the built-in cloud helpers are built, and in which phase;
  [#138](https://github.com/tran4774/ranikust/issues/138) tracks them in the
  Backlog, and an ADR then decides it.
- Whether a base image extracted on `/` counts like a `RUN` for the rule that
  refuses the external `git` and `ssh` there [open: #159].

Follow-ups, with Phase 5's tasks and not before they are built: the
`rnk-registry` and `ranikust` cards, the book's usage page (the credential
section and the mirror flags), the kaniko map, the threat model (T11, and a row
for helpers), and the `## Changelog` entry. The code's message for
`--credential-helpers` (`crates/ranikust/src/cli.rs`) says Phase 5; since this
ADR it is a later phase, in the Backlog as #138.
