# Rules

**TL;DR:** The must-follow list; no lint catches these. Host-safety mistakes
damage the machine you run on; determinism mistakes break reproducible
digests. If a rule blocks the task, stop and ask.

## Host safety

- ranikust runs `RUN` as root, mounts and empties build roots. Never run it
  in kaniko mode with rootfs `/` on the host: it deletes the host's files.
  Use a `--workdir` under a fresh temp dir, or the VM tier.
- Never use `sudo`, `pkexec`, `su`, `doas`, `mount`, `umount`, `unshare` or
  `nsenter`. If a task needs root, stop and say so.
- Privileged tests (tier T2) run only in a disposable VM, never on the host.
- Every path from an image, a build context or a Dockerfile is untrusted:
  reach it only through `rnk_rootfs::RootPath`, never `std::fs` or
  `Path::join` onto a host path.
- The deny rules and the rustfmt hook in `.claude/` are guardrails, not a
  sandbox: follow these rules whether or not a command is blocked.

## Security invariants

From [`ARCHITECTURE.md`](../../ARCHITECTURE.md#cross-cutting-invariants) and the [threat model](../security/threat-model.md#5-threats-and-mitigations) (T-ids).

- Write only to the workdir, the state directory or store, and the outputs.
- Delete by file descriptor, never following symlinks or crossing a mount
  point (mount id, never `st_dev` alone; T4).
- Workdir outside a container: not `/`, no system directory inside (by
  `(dev, ino)`), `.ranikust-root` marker if not empty; locked in kaniko mode.
  There the state directory is on the ignore list (`ADR-0022`).
- `RUN` enters via `pivot_root` (`MS_MOVE` + `chroot` if seccomp blocks it);
  bare `chroot` only with `--parallel-stages` (T5). Bounding and inheritable
  capabilities drop to Docker's defaults for every `USER` (T6).
- `USER` from the root's `/etc/passwd`/`/etc/group` (`rnk-rootfs`), never
  the host's; every descriptor `O_CLOEXEC`, none reaches `RUN` (T7).
- Credentials and secrets never reach logs, layers, the config, cache keys,
  errors or the `RUN` environment; a secret is a file mounted only for the
  `RUN` that asks (T11).
- Store entries are digest-verified and published by atomic rename (T13).
- Errors are never dropped; probe and mount failures report step and errno.

## Code traps

- No global state: pass `BuildPaths` (`rnk-store`) and config values down;
  no process-wide current directory, `PATH` or environment change.
- No architecture-specific code; syscall numbers only via `rustix`/`libc`.
  Prefer `rustix` to raw `libc`.
- `unshare` only while single-threaded, before tokio or rayon start. Fork
  `RUN` children from a thread that lives for the whole build, never from
  `spawn_blocking`: `PR_SET_PDEATHSIG` fires when that thread exits.
- Release builds abort on panic: no `unwrap`, `expect` or indexing that
  untrusted input can reach, and no cleanup that relies on `Drop`. Journal
  what must be undone before doing it.
- No `let _ =` or `.ok()` on a fallible call without a comment saying why.
- No `_` arm when matching our own enums: a new variant must force a decision.
- Whatever ranikust puts into a rootfs (stubs, `resolv.conf`, fake `/proc`)
  is removed and its parent's metadata restored before the diff.
- A new dependency or a new `pub` item at a crate root is an architecture
  change: justify it in the PR.
- New code instruments as [`design/metrics.md`](../design/metrics.md#emitting-spans)
  says: a `debug_span!` named `<crate>.<operation>` per layer, blob,
  instruction or stage, never a span or timer per file or entry (count in
  the loop, record the total once); numeric fields carry their unit in the
  name (`bytes_in`, `files`, `user_ns`); no library depends on
  `rnk-metrics`.

## Determinism

- Layer contents and digests never depend on `--jobs`, thread count,
  scheduling, input chunking or the host's users. Parallel gzip writes one
  member; zstd writes one independent frame per 1 MiB, never libzstd's
  multithreading (`exp-05`).
- Timestamps follow kaniko: the current time unless `--reproducible`, which
  uses a fixed epoch or `SOURCE_DATE_EPOCH`.
- Tested: with `--reproducible`, `--jobs 1` and the default give the same
  digests; overlay and kaniko mode give the same flattened content apart
  from `ADR-0012`'s accepted differences. Cache keys ignore mode, host, time,
  thread count.

## Tests

- A bug fix comes with a test that fails without it.
- T0 tests need no privileges or network and write only below a fresh temp
  dir, cleaned up on failure too. Tests needing user namespaces or root are
  `#[ignore = "T1: ..."]`/`"T2: ..."`; never un-ignore them or run T2 on the
  host. See [`docs/testing.md`](../testing.md#rules).

## Branches and commits

- Start every new piece of work (feature, fix, refactor, docs) from the
  latest `main`, on its own branch, never on `main` itself:
  `git switch main && git pull --ff-only && git switch -c <type>/<scope>-<topic>`,
  for example `feat/rnk-oci-types` or `fix/rnk-rootfs-symlink-escape`.
- Commit at every checkpoint: a coherent step that builds, passes
  `just check` and already has its docs updated. Conventional Commits
  ([workflow.md](workflow.md#commits)); small commits are fine, pull requests
  are squash-merged.
- Push, open a pull request or merge only when the maintainer asks. Never
  force-push.

## Open questions

- Never guess a decision that no ADR or design doc makes. Add it to
  [`docs/open-questions.md`](../open-questions.md) as the next `Q-NNN`, mark
  the spot `[open: Q-NNN]`, tell the maintainer, and continue with work that
  does not depend on it, or with the smallest reversible choice, recorded as
  such in the entry.
- Once answered: record the answer in the entry, update the ADR or design
  doc that owns it, and remove the `[open: Q-NNN]` markers.

## Keep in sync

**Mandatory.** Work is not done until every document it affects, in `docs/`
and in the repository root, is updated on the same branch, ideally in the same
checkpoint commit. When nothing needs updating, say so in the commit body.
`/sync-docs` checks most rows.

| Change | Update |
|---|---|
| Crate added, removed or responsibility moved; API or status changed | `ARCHITECTURE.md` and the crate's `# Boundaries` doc (structure), its card in [crates/](crates/), the [README.md](README.md) table |
| Design changed | the matching `docs/design/*.md` |
| Decision made or reversed | a new ADR; never rewrite an accepted one |
| User-visible change (flag, default, output, mode behaviour) | `CHANGELOG.md` under Unreleased, and the book |
| Isolation or privilege behaviour | `SECURITY.md`, `docs/security/` |
| Test tiers, recipes or CI jobs | `justfile` and `ci.yml` identical; `docs/testing.md`, `CONTRIBUTING.md`, the command lists in `CLAUDE.md` and [workflow.md](workflow.md) |
| Code ported from kaniko or elsewhere | file header, `NOTICE` entry, source commit ([CONTRIBUTING.md](../../CONTRIBUTING.md#porting-code-from-kaniko)) |
| Phase 1 task finished | tick it in [phase-1.md](phase-1.md) |
| Span or span field added, renamed or removed | [`design/metrics.md`](../design/metrics.md#spans-per-crate) and the card's `## Metrics` |
| New measurement | a new experiment report or ADR evidence; `docs/research/` is frozen |

## Writing

- English for code, comments, docs and commits. Conventional Commits
  (`feat(rnk-rootfs): ...`); git-cliff builds release notes from them.
- Write for a reader who never saw your session: no conversation history or
  "as discussed" in code, commits or PRs.
- Claims about kaniko cite `kaniko@<commit> path:line`.
- Hard-wrap Markdown prose at about 80 columns, not tables, headings, links
  or code. No email addresses anywhere in the repository.

## Agent conduct

- Never edit this file or `CLAUDE.md` on your own; propose rules under
  "Suggested agent rule additions" in the PR description.
- Durable decisions go into an ADR, not auto memory. No unattended issues,
  PRs or review replies. `[open: Q-NNN]` means ask, do not invent.
