---
name: buildkit-parity
description: Compare ranikust behaviour with BuildKit's Go source and cite path:line. Use when an open question or a design choice is settled by what BuildKit does (ADR-0032, ADR-0049: BuildKit breaks ties between options equal on security and performance), or when asked what BuildKit does: Dockerfile semantics, COPY/ADD, RUN environment and mounts, image config, media types, overlay diffs, cache keys, contexts.
allowed-tools: Read Grep Glob
---

# Parity with BuildKit

ranikust decides undecided behaviour by security first, then performance;
between options equal on both, BuildKit's observable behaviour decides
(`ADR-0049`, refining `ADR-0032`; decisions already taken under ADR-0032
stand). A claim about what BuildKit does is only valid with a citation into
BuildKit's source.

## Find the source

- The checkout is `../buildkit` from the repository root (a clone of
  moby/buildkit). Resolve it to an absolute path; Claude needs read access
  (`--add-dir <path>` or `/add-dir <path>`).
- If it is missing, say so and stop. Do not answer from memory; at most a
  labelled `[inferred]` guess and what to check.
- Record the commit you read (`<path>/.git/HEAD`, the ref it names or
  `.git/packed-refs`). ADR-0032 and ADR-0049 cite `buildkit@cd8ee244`;
  if the checkout moved, say which commit you read and whether the cited
  files changed.

## Where things are in BuildKit

| Topic | Look in |
|---|---|
| Dockerfile parser, escapes, heredocs | `frontend/dockerfile/parser/`, `frontend/dockerfile/shell/` |
| Instructions and their flags | `frontend/dockerfile/instructions/` (`parse.go`, `commands.go`) |
| Dockerfile to LLB: stages, `COPY`, `ADD`, `RUN`, `ONBUILD`, `ARG`/`ENV` | `frontend/dockerfile/dockerfile2llb/convert.go`, `convert_runmount.go`, `convert_secrets.go`, `convert_ssh.go` |
| Build options, contexts, `.dockerignore` | `frontend/dockerui/` |
| `COPY` semantics (chown, chmod, wildcards, symlinks) | `solver/llbsolver/file/`, `solver/llbsolver/ops/file.go`, vendored `github.com/tonistiigi/fsutil` |
| `RUN` environment, hosts, resolv.conf, network | `executor/oci/`, `executor/runcexecutor/`, `solver/llbsolver/ops/exec.go` |
| Overlay diffs and layers | `util/overlay/overlay_linux.go`, `snapshot/`, vendored containerd `pkg/archive/` |
| Image config, media types, compression | `exporter/containerimage/`, `util/compression/` |
| Cache keys | `solver/`, `cache/`, `solver/llbsolver/ops/` (`CacheMap`) |
| Git, HTTP contexts | `util/gitutil/`, `source/git/`, `source/http/` |
| Push, registries | `util/push/`, `util/resolver/`, vendored containerd `core/remotes/` |
| GC and prune defaults | `cmd/buildkitd/config/` |
| Expected results | `*_test.go` next to the code; `frontend/dockerfile/dockerfile_test.go` (integration) |

## Method

1. Grep for the entry point, then read the whole function and its callers.
   Note branches that depend on a frontend option, a `BUILDKIT_*` variable,
   a capability or a feature flag.
2. Find the tests that pin the behaviour.
3. Compare with ranikust's code or design (`docs/design/`).
4. Report:
   - BuildKit: what it does, with `buildkit@<commit> path:line`;
   - ranikust: what it does or should do, with `path:line`;
   - verdict: same / intended divergence (cite the ADR) / unintended
     divergence, a bug or a question for a new ADR; and, if the question is
     open, whether security or performance already decide it before
     BuildKit does.

For broad questions, delegate to the `upstream-oracle` subagent with the
resolved path instead of reading in the main session. If
`<path>/graphify-out/graph.json` exists, run `graphify query "<question>"
--graph <path>/graphify-out/graph.json` first and pass the output.

Reading BuildKit to learn its behaviour is fine. Translating its code
closely makes the result a derivative of Apache-2.0 code: follow "Porting
code from kaniko" in `CONTRIBUTING.md`, which applies to BuildKit too (file
header, `NOTICE` entry, source commit).
