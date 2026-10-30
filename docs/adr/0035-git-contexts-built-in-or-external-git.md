---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0035: Git contexts use a built-in git by default, an external `git` on request, with its flags configurable

## Context and Problem Statement

A git build context (`--context git://…`, Phase 5) must be cloned. BuildKit
runs the `git` binary; kaniko links go-git. ranikust is one static musl
binary that must work in `scratch` (`ADR-0020`), where there is no `git`,
so BuildKit's way cannot be the only one (`ADR-0032` rule B1). Users who
need git features a library lacks, or their own git set-up (proxies, SSH,
credential helpers, `url.<base>.insteadOf`), need an external `git` and a
place to configure it. kaniko reads the fragment `#ref#commit`, BuildKit
`#ref:subdir`. [Q-045](../open-questions.md#q-045-how-are-git-contexts-fetched-and-addressed).

## Considered Options

- (a) A built-in Rust git implementation (`gix`) only: works in `scratch`,
  but nothing for users who need their own git.
- (b) An external `git` only, refusing when absent: fails in `scratch`, the
  usual kaniko set-up.
- (c) Both: the built-in by default, an external `git` on request, with its
  flags configurable.

## Decision Outcome

Git contexts are cloned by a built-in `gix` backend by default; the
operator may choose an external `git` binary instead, configured by the
flags and variables below.

Backend and its configuration (flag, then variable; the flag wins):

| Flag | Variable | Meaning |
|---|---|---|
| `--git-backend=builtin\|external` | `RANIKUST_GIT_BACKEND` | Which backend clones. Default `builtin`. No silent fallback between them [open: Q-250]. |
| `--git-binary=PATH` | `RANIKUST_GIT_BINARY` | The external binary; default `git` looked up in `PATH`. Only with `external`. |
| `--git-config KEY=VALUE` (repeatable) | `RANIKUST_GIT_CONFIG` (`;`-separated) | Extra git configuration. External: passed through `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`, not on the command line. Built-in: applied as `gix` configuration overrides; a key the built-in does not understand is an error naming `--git-backend=external` [open: Q-252]. |
| `--git-arg ARG` (repeatable) | none | Extra global options for the external `git`, placed before the subcommand. Each must start with `-`; `-c`, `-C`, `--git-dir`, `--work-tree`, `--exec-path`, `--namespace` and `--config-env` are refused (use `--git-config`). External only. |
| `--git-host-config` | `RANIKUST_GIT_HOST_CONFIG=1` | Let the external `git` read the host's system and user configuration (`HOME`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` passed through), as BuildKit's `WithHostGitConfig`. Default off: the configuration is isolated. |
| `--git KEY=VALUE` (repeatable) | none | kaniko's flag, same keys and meaning (`ADR-0011`): `branch`, `single-branch`, `depth`, `recurse-submodules`, `insecure-skip-tls`; both backends. |
| none | `GIT_USERNAME`, `GIT_PASSWORD`, `GIT_TOKEN`, `GIT_PULL_METHOD` | kaniko's variables, same meaning (`ADR-0011`); both backends. For the external `git` the credentials go in an `http.extraHeader` set through `GIT_CONFIG_*`, never in the URL or argv. |

The external `git` runs with BuildKit's environment and hardening:

- Environment: only `PATH`, `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`, the
  proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, `ALL_PROXY`
  and their lower-case forms), `SSH_AUTH_SOCK` when set, and
  `GIT_SSH_COMMAND=ssh -F /dev/null` with host-key checking as BuildKit;
  `GIT_CONFIG_NOSYSTEM=1`, `HOME=/dev/null` and
  `GIT_CONFIG_GLOBAL=/dev/null` unless `--git-host-config`. A different SSH
  command is set with `--git-config core.sshCommand=…`.
- Always passed, after the operator's configuration so it cannot be
  undone: `protocol.file.allow=user` (BuildKit), `protocol.ext.allow=never`,
  `core.hooksPath=/dev/null`, `core.fsmonitor=false`.
- The repository URL, the ref and the commit come from the context
  argument, which may be untrusted (a CI variable): they are parsed by
  ranikust, a ref must pass `git check-ref-format` rules and a commit must
  be hex, none may start with `-`, and they are passed after `--`. Nothing
  from the URL becomes a `-c` option.
- The clone goes into the store's temporary directory and is read through
  `RootPath` like any context; `.git` is removed unless BuildKit's
  `BUILDKIT_CONTEXT_KEEP_GIT_DIR=1` build argument is given.

The built-in backend:

- Trusts the same roots as the registry client (host CAs plus webpki,
  `ADR-0024`); `--git insecure-skip-tls=true` disables verification as in
  kaniko. Protocols `https`, `http` (with `GIT_PULL_METHOD=http` or an
  explicit `http://`) and `ssh` (through the `ssh` binary, as `gix` does)
  [inferred].
- Submodules follow `--git recurse-submodules`.

Fragments, both forms (`ADR-0011` for kaniko's, `ADR-0032` for BuildKit's):

- `#ref:subdir` (BuildKit and Docker) and the query form
  `?ref=…&subdir=…`;
- `#ref#commit` (kaniko): a second `#` selects kaniko's form; a commit hash
  in the first position is fetched and checked out, as kaniko does.
  Combining kaniko's commit with a subdirectory is [open: Q-251].

Not covered: `ADD <git>` (`rnk-builder`, Phase 5) uses the same backend
and options; credential helpers for registries (Q-047) are separate.

### Consequences

- Good, because builds from git work in `scratch` with nothing installed.
- Good, because users with special git set-ups keep them through
  `--git-backend=external`, `--git-config` and `--git-host-config`.
- Good, because the external `git` is isolated and hardened as BuildKit's,
  plus hooks and `fsmonitor` turned off, so an untrusted repository or URL
  cannot run code on the host.
- Bad, because two backends must be tested against the same goldens, and
  the built-in may lag behind git for rare features.
- Bad, because `gix` adds to the binary size (to be measured).

## Evidence

- kaniko: `--git` takes `key=value` for `branch`, `single-branch`, `depth`,
  `recurse-submodules`, `insecure-skip-tls`
  (`kaniko@44aa7ddc0 pkg/config/options.go:106-158`,
  `cmd/executor/cmd/root.go:368`); `GIT_PULL_METHOD`, `GIT_USERNAME`,
  `GIT_PASSWORD`, `GIT_TOKEN` (`pkg/buildcontext/git.go:37-45`, `:197-220`);
  the fragment `#ref#commit` with refs, pull-request refs and commit hashes
  (`git.go:56-150`) [code].
- BuildKit runs the `git` binary, path configurable (`buildkit@cd8ee244
  util/gitutil/git_cli.go:41-46`, `:163-166`), with `-c
  protocol.file.allow=user` (`:185-186`), an environment of `PATH`,
  `GIT_TERMINAL_PROMPT=0`, `GIT_SSH_COMMAND`, `LC_ALL=C` (`:231-237`), the
  host configuration only on request, else `GIT_CONFIG_NOSYSTEM=1`,
  `HOME=/dev/null`, `GIT_CONFIG_GLOBAL=/dev/null` (`:245-272`), the proxy
  variables (`:167-170`, `:273-277`), `SSH_AUTH_SOCK` (`:278-280`) and
  `ssh -F /dev/null` (`:363-371`) [code].
- BuildKit's fragment is `#ref:subdir` (`util/gitutil/git_url.go:71-79`),
  the query is kept (`:112-128`), protocols `http`, `https`, `ssh`, `git`
  (`:25-30`); `BUILDKIT_CONTEXT_KEEP_GIT_DIR` keeps `.git`
  (`frontend/dockerui/config.go:59`, `context.go:87`, `:302-306`,
  `source/git/source.go:297`) [code].
- `GIT_CONFIG_COUNT` configuration through the environment needs git 2.31
  or later [external].
- `gix` configuration overrides and SSH through the `ssh` binary
  [inferred, to check when implementing].

## More Information

- Answers Q-045 (escalated under `ADR-0032`). Related: `ADR-0011` (kaniko
  flags), `ADR-0020` (static binary), `ADR-0024` (TLS roots), `ADR-0032`.
- Design: [rnk-context](../agents/crates/rnk-context.md#phase-1) (Phase 5
  plan), [kaniko-map](../agents/kaniko-map.md#flags).
- Open: Q-250 (fallback between backends), Q-251 (kaniko's commit with a
  subdirectory), Q-252 (which `--git-config` keys the built-in honours).
- Revisit when `gix` is measured (binary size, clone speed against `git`).
