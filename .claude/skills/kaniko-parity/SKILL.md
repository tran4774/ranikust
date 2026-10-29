---
name: kaniko-parity
description: Compare ranikust behaviour with kaniko's Go source and cite path:line. Use when implementing or reviewing anything that should behave like kaniko (Dockerfile instructions, flags, snapshots, ignore lists, layer contents, image config, cache) or when asked what kaniko does.
allowed-tools: Read Grep Glob
---

# Parity with kaniko

ranikust aims to build what kaniko builds and to accept kaniko's common flags
(ADR-0011). It does not share kaniko's cache (ADR-0005), and its two snapshot
modes may differ from each other in accepted ways (ADR-0012). A claim about
what kaniko does is only valid with a citation into kaniko's source.

## Find the source

- The kaniko checkout is `$KANIKO_SRC`, default `../kaniko` from the
  repository root. Resolve it once (`echo "${KANIKO_SRC:-../kaniko}"`) and
  use the absolute path. Claude needs read access to it: start the session
  with `--add-dir <path>` or run `/add-dir <path>`.
- If the checkout is missing, say so and stop. Do not answer from memory;
  at most, give a clearly labelled `[inferred]` guess and what to check.
- Record the commit you read (`<path>/.git/HEAD` and the ref it names). The
  research reports cite `kaniko@44aa7ddc0`, a local branch of 13 performance
  commits on top of osscontainertools/kaniko `34ba9eadf` (v1.28.6 + 7); see
  `docs/research/README.md`. Those commits are performance work. Prefer
  behaviour that upstream `34ba9eadf` also has, and say so when a cited file
  was touched by the branch (`git log 34ba9eadf..HEAD -- <file>` in the
  checkout).

## Where things are in kaniko

| Topic | Look in |
|---|---|
| Flags and defaults | `cmd/executor/cmd/root.go`, `pkg/config/` |
| Stage loop, squashing, cross-stage deps | `pkg/executor/build.go`, `pkg/dockerfile/` |
| One file per instruction | `pkg/commands/*.go` (`run.go`, `copy.go`, `add.go`, `user.go`, ...) |
| Snapshots, hashing, whiteouts | `pkg/snapshot/`, `pkg/util/fs_util.go` |
| Ignore list, `DeleteFilesystem`, extraction | `pkg/util/fs_util.go` |
| Cache keys and lookup | `pkg/cache/`, `pkg/executor/` |
| Push, tar and OCI layout output | `pkg/executor/push.go` |
| Dockerfile parsing (BuildKit) | `vendor/github.com/moby/buildkit/frontend/dockerfile/` |
| `.dockerignore` matching | `vendor/github.com/moby/patternmatcher/` |
| Expected results | `*_test.go` next to the code; `integration/dockerfiles/` (200 Dockerfiles and one `.dockerignore`) |

## Method

1. Grep for the entry point, then read the whole function and its callers.
   Note every branch that depends on a flag, a feature flag (`FF_KANIKO_*`)
   or an environment variable.
2. Find the tests and integration Dockerfiles that pin the behaviour.
3. Compare with ranikust's code or design (`docs/design/`).
4. Report:
   - kaniko: what it does, with `kaniko@<commit> path:line` (ranges for
     longer passages);
   - ranikust: what it does or should do, with `path:line`;
   - verdict: same / intended divergence (cite the ADR or the accepted
     differences of ADR-0012) / unintended divergence, which is a bug or
     needs a new ADR.

For broad questions that need a lot of reading, delegate to the
`kaniko-oracle` subagent and pass it the resolved path. If
`<path>/graphify-out/graph.json` exists, run
`graphify query "<question>" --graph <path>/graphify-out/graph.json` first
and include the output in its prompt so it starts at the right files.

Reading kaniko to learn its behaviour is fine. Translating its code closely
makes the result a derivative of Apache-2.0 code: follow "Porting code from
kaniko" in `CONTRIBUTING.md` (file header, `NOTICE` entry, source commit).
