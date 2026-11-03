# Contributing to ranikust

Thank you for your interest. ranikust is pre-alpha: it builds images as
root, in overlay mode or in kaniko mode, and its releases are pre-releases;
rootless builds, the layer cache and more instructions come in later phases
(the [roadmap](README.md#roadmap)). Before starting anything larger than a
small fix, open an issue so the work fits the plan in
[`ARCHITECTURE.md`](ARCHITECTURE.md) and the decisions in
[`docs/adr/`](docs/adr/).

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
Security problems go through [`SECURITY.md`](SECURITY.md), never a public
issue.

## Development setup

ranikust builds and runs on Linux only. x86_64 is the release target; aarch64
must keep compiling ([ADR-0009](docs/adr/0009-x86-64-first-aarch64-ready.md)).
[docs/development-setup.md](docs/development-setup.md) installs everything
step by step, zig included; the table says what each tool is for. Nothing in
the build needs root, and nothing in the dev loop should ever be run with
`sudo`.

| Tool | Why | Install |
|---|---|---|
| [rustup](https://rustup.rs/) | Reads [`rust-toolchain.toml`](rust-toolchain.toml) and installs the pinned compiler, `rustfmt`, `clippy`, `rust-analyzer` and both musl targets on the first `cargo` command | your distribution, or rustup.rs |
| [just](https://github.com/casey/just) | Runs the recipes in [`justfile`](justfile) | `cargo install --locked just` |
| [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) | License, advisory and source checks | `cargo install --locked cargo-deny@0.20.2` |
| [mdBook](https://github.com/rust-lang/mdBook) | Builds the user guide in `docs/book` | `cargo install --locked mdbook@0.5.4` |
| Python 3.11 or later | `just docs-lint`, `just review-pack`, and the release scripts (`just release-prepare`, `just test-release`; `tomllib`) | your distribution |
| [GitHub CLI](https://cli.github.com/) (`gh`) | `just release-prepare` reads the merged pull requests; checking a release's attestation needs 2.102.0 or later | your distribution, or cli.github.com |
| [cargo-about](https://github.com/EmbarkStudios/cargo-about) | Optional: `just third-party-licenses` writes the licence notices the release ships (ADR-0094); the release workflow installs its own | `cargo install --locked cargo-about@0.9.2` |
| [git-cliff](https://git-cliff.org/) | Optional: `just changelog` drafts notes from commit messages; releases are drafted from pull requests by `just release-prepare` | `cargo install --locked git-cliff` |
| `file`, `readelf` | `just build-static` and `just test-aarch64` check the binary is static | `file` and `binutils` packages |
| A C compiler as `cc` | Builds the C dependencies (libzstd, mimalloc, ring) for the host, and links the static binary | `gcc` or `clang` package |
| [zig](https://ziglang.org/) 0.16.0 | Compiles the C dependencies for the musl targets in `just build-static`, `just check-aarch64` and `just test-aarch64` ([ADR-0020](docs/adr/0020-static-musl-via-cargo-zigbuild.md)) | [docs/development-setup.md](docs/development-setup.md#1-the-build-every-contributor) |
| [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) | Hands zig to those builds | `cargo install --locked cargo-zigbuild@0.24.0` |
| `aarch64-linux-gnu-gcc` | Links the aarch64 binary as static-pie in `just test-aarch64` on x86_64 (on aarch64 the native `cc` does); without it the recipe links with zig and cannot check static-pie (Q-140) | `gcc-aarch64-linux-gnu` package |
| `qemu-aarch64` (QEMU user mode) 9.1 or later | `just test-aarch64`, so `just ci`, on an x86_64 machine: the aarch64 tests under emulation (8.2 has no `openat2`); a binfmt_misc entry also runs ranikust's own CLI tests. Not needed on aarch64, where they run natively (as in CI) | `qemu-user` package if 9.1 or later, else Debian 13's `qemu-user` unpacked with `dpkg -x`; `QEMU_AARCH64=<path>` |
| rootless [Podman](https://podman.io/) | `just smoke` and `just smoke-case` (the T1 smoke tests, [docs/testing.md](docs/testing.md)), `just test-vm e2e` and `ultra` (they save the reference images from it), and `just registry-up` (`podman compose`, with `openssl`, python3's `bcrypt` and `skopeo`) | `podman`, `podman-compose` or `docker-compose`, `python3-bcrypt`, `skopeo` packages |
| A [`nextjs-kaniko`](docs/testing.md#end-to-end-workloads-e2e) checkout next to the repository, with its registry and verdaccio storage filled and its images in podman | Only `just test-vm ultra`, the ultra benchmark; read, never written | `RANIKUST_NEXTJS_KANIKO=<path>` |
| QEMU 10.1 or later with KVM and its passt backend, [passt](https://passt.top/) 2024_10_30 or later, `xorriso`, `jq`; `socat` for the `push` suite | Only `just test-vm`, the T2 VM harness; `/dev/kvm` must be readable and writable by you (its group or an ACL), never through `sudo`. passt is the VM's egress ([ADR-0086](docs/adr/0086-the-t2-vm-reaches-no-host-loopback-port-but-the-test-registries.md)) | `qemu-system-x86`, `qemu-utils`, `passt`, `xorriso`, `jq`, `socat` packages |

The versions of cargo-deny, mdBook, zig and cargo-zigbuild match CI.

## Development loop

```sh
just                      # list the recipes
cargo test -p rnk-rootfs   # narrow: the crate you changed (add a test name filter)
just check                # fmt check + clippy -D warnings + all T0 tests
just ci                   # everything CI runs on a full run, before you open a pull request
```

Also run `just check-aarch64` after touching syscalls or `cfg`, `just deny`
after changing dependencies, and `just doc` / `just book` after changing
documentation; `just docs-lint` (needs python3; CI runs it on every pull
request) after an ADR, a marker or an open question changes. Its check of
the `[open: #N]` markers needs `gh` installed and logged in (`gh auth
login`) with network access; without them it is skipped locally, and the
last line says so. Every recipe that mirrors CI runs exactly the command
of the CI job; if you change one, change the other. A new top-level path,
or a file a CI job starts to read, needs its lane in `scripts/ci.py`
(`just test-ci` fails otherwise; see below).

### What CI runs

CI picks its jobs from what a pull request changes
([ADR-0095](docs/adr/0095-ci-runs-by-lanes-a-fast-lane-on-drafts-and-the-full-run-when-ready-behind-two-gate-checks.md);
the lanes and rules are in [docs/testing.md](docs/testing.md#ci-lanes)):

- **A draft pull request** gets the fast lane: rustfmt, clippy, the x86_64
  tests and the docs when Rust code changed, cargo-deny when the manifests
  or `deny.toml` changed, mdBook when the book changed, and docs-lint
  always. A pull request that changes only documentation runs little more
  than docs-lint.
- **A full run** adds the static release build, the native aarch64 tests
  and the privileged T2 jobs, when the change needs them. It runs when the
  pull request is marked ready for review, when a maintainer adds the
  `ci:full` label to a draft, from "Run workflow" in the Actions tab
  (`workflow_dispatch`, any branch), and on every push to `main`.
- **The required checks** are two gates, `CI result` and `T2 result`, which
  fail if any job failed or was cancelled. A draft's or a manual run's gates
  carry other names (`... (draft, fast lane)`, `... (manual run)`), so a
  pull request is mergeable only after its own full run.

[`docs/agents/rules.md`](docs/agents/rules.md) lists the traps the lints do
not catch and the host-safety rules. Read it even if you do not use an AI
agent: it is the short version of how to write code here, comments and docs
included ([Writing](docs/agents/rules.md#writing)).

## Tests

Tests are split into tiers by the privileges they need
([docs/testing.md](docs/testing.md) has the harnesses, the CI lanes and the
rules):

| Tier | What | Where |
|---|---|---|
| T0 | Unit and in-process tests | Anywhere: `just test` |
| T1 | Rootless integration (user namespaces, rootless Podman); today the smoke tests only | Your machine: `just smoke` |
| T2 | Privileged integration: real root, overlay, rootful Docker, host-safety attacks (T2c: the root tests that mean the same in a throwaway container) | A disposable VM only: `just test-vm`; on a full CI run, a GitHub-hosted runner |
| T3 | CI: format, clippy, T0, cargo-deny, docs, docs-lint, the static build, the native aarch64 tests | GitHub Actions, by lanes ([above](#what-ci-runs)) |

Tests above T0 are marked `#[ignore = "T1: ..."]`, `"T2c: ..."` or
`"T2: ..."` and run only through their harness. **Never run a T2 or T2c
test, or ranikust in kaniko mode, on your own machine**: kaniko mode empties
its root filesystem, and the rest needs real root
([rules](docs/agents/rules.md#host-safety)). A bug fix comes with a test
that fails without the fix.

## Commits and pull requests

Start every change from the latest `main` on its own branch, named
`<type>/<scope>-<topic>` (for example `feat/rnk-oci-types`):

```sh
git switch main && git pull --ff-only && git switch -c feat/rnk-oci-types
```

Commit at each checkpoint, a step that builds and passes `just check` with
its documentation already updated. Updating the affected documentation is
part of every change, not a follow-up. Questions the design does not answer
become an issue with the **Design question** form, labelled
[`question`](https://github.com/tran4774/ranikust/issues?q=is%3Aissue+label%3Aquestion),
instead of being guessed. A new question is named by its issue number and
takes no `Q-NNN` number
([ADR-0091](docs/adr/0091-a-new-design-question-is-named-by-its-issue-number.md));
the 29 questions migrated from
[`docs/open-questions.md`](docs/open-questions.md) keep their `Q-NNN:`
title prefix, and that file keeps the answered ones.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).
The release notes come from the pull requests' `## Changelog` sections
(below), not from commits; the type of a pull request's title only places
an entry written without a heading, as the table shows. (`just changelog`,
with [git-cliff](cliff.toml), can still draft notes from commits.)

```
<type>(<scope>): <summary in the imperative, lower case, no full stop>

<body: what and why, wrapped at 72 columns>

<footers: BREAKING CHANGE: ..., Refs: #123>
```

| Type | Use for | An entry without a heading goes under |
|---|---|---|
| `feat` | New user-visible behaviour | Added |
| `fix` | Bug fix | Fixed |
| `perf` | Faster, same behaviour | Changed |
| `refactor`, `test`, `docs`, `ci`, `build`, `chore`, `style` | Everything else | Usually no entry (`none`); Changed otherwise |

The scope is the crate name (`rnk-rootfs`, `ranikust`) or an area (`ci`,
`book`, `adr`). Mark breaking changes with `!` after the type or a
`BREAKING CHANGE:` footer; use the `security` scope (or the `security`
label) for security fixes, whose entries go under Security. Summaries
starting with "deprecate" or "remove" go under Deprecated and Removed.

Pull requests are squash-merged, so the pull request title becomes the
commit message and must follow the same format. The
[pull request template](.github/PULL_REQUEST_TEMPLATE.md) asks for a summary
with the issue it closes, a changelog entry, a test plan, the docs you
updated and whether AI tools helped. A pull request keeps the documentation
in sync in the same change, following the "Keep in sync" list in
[`docs/agents/rules.md`](docs/agents/rules.md#keep-in-sync).

**The `## Changelog` section.** A user-visible change gets its entry in the
pull request's `## Changelog` section, not in
[`CHANGELOG.md`](CHANGELOG.md), which is written from those sections when a
release is prepared ([ADR-0088](docs/adr/0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md)).
Write the entry as bullets for users under Keep a Changelog headings
(`### Added`, `### Changed`, `### Deprecated`, `### Removed`, `### Fixed`,
`### Security`), or as bullets alone, placed by the title's type; write
`none` when users see no change. `just release-prepare` reads exactly this.

Planned work is on the
[Project](https://github.com/users/tran4774/projects/1) and the phase
milestones; issues labelled `good first issue` are a good start.

## Releasing

Releases follow
[ADR-0089](docs/adr/0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md).
Before 1.0 every release is a pre-release: a minor version marks the end of
a phase, a patch version ships fixes and user-visible changes once `main` is
green, the privileged T2 tests included. A release is one static
`x86_64-unknown-linux-musl` binary in a tarball, with the licence notices of
the code it contains (`THIRD-PARTY-LICENSES`,
[ADR-0094](docs/adr/0094-the-release-ships-third-party-licence-notices-generated-by-cargo-about.md)),
`SHA256SUMS` and a GitHub build provenance attestation; there is no
container image and no aarch64 build yet.

**Before the first tag** (the maintainer, once): the repository needs an
environment named `release` with the maintainer as required reviewer and
deployments limited to the tag rule `v*` (Settings → Environments;
[ADR-0092](docs/adr/0092-a-release-is-verified-by-workflow-and-tag-and-built-without-cache-or-unprotected-environment.md)).
Leave "Prevent self-reviews" off: the maintainer pushes the tag and is the
only reviewer, so with it on nobody could approve the release. A job that
names a missing environment creates it with no protection, so the workflow
checks it (`scripts/release.py check-environment`) before it builds and
again before it publishes, and stops a tag otherwise; a pull request's dry
run reports what it found.

1. **Release pull request** (anyone, agents included; an agent asks before
   any tag or release,
   [ADR-0087](docs/adr/0087-work-flows-through-github-issues-pull-requests-and-a-project.md)).
   From the latest `main`, on a branch `release/vX.Y.Z`, run
   `just release-prepare X.Y.Z`. It sets the workspace version, refreshes
   `Cargo.lock` offline and drafts the `## [X.Y.Z]` section of
   `CHANGELOG.md` from the `## Changelog` sections of the pull requests
   merged since the last tag
   ([ADR-0088](docs/adr/0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md)),
   listing the ones without an entry. Edit the section for users (what
   changed for them, not how) and wrap it at about 80 columns, delete the
   draft comment, commit it as `chore(release): vX.Y.Z` and open the pull
   request with the `release` label. The recipe never commits, tags or
   pushes.
2. **Merge** (the maintainer), once CI and the T2 tests are green; the
   `main` ruleset accepts changes only through a pull request whose gates,
   `CI result` and `T2 result`, passed on a full run.
3. **Tag** (the maintainer only: the `release tags` ruleset lets only the
   maintainer create, move or delete `v*` tags): tag the merge commit and
   push the tag.

   ```sh
   git switch main && git pull --ff-only
   git tag -s vX.Y.Z -m "ranikust X.Y.Z"   # -a instead of -s without a signing key
   git push origin vX.Y.Z
   ```

4. **Build** (the `Release` workflow, `.github/workflows/release.yml`). It
   tests its scripts (`just test-release`); checks that the tag equals the
   workspace version and lies on `main`, that `CHANGELOG.md` has a finished
   section for it, and that the `release` environment is protected; builds
   the binary from the tag without any cache (Zig downloaded with a pinned
   checksum); checks it is static-pie, runs and reports the version
   (`scripts/check-static.sh`, as CI and `just build-static` do); packages
   it reproducibly (`scripts/release.py package`) with
   `THIRD-PARTY-LICENSES` (a job of its own runs cargo-about on `Cargo.lock`
   for the release target, stopping on a licence `deny.toml` does not allow,
   and adds the toolchain's notices from `licenses/`; `just
   third-party-licenses` does the same locally); and attests the tarball's
   build provenance in a job of its own. When `rust-toolchain.toml` changes,
   replace the files in `licenses/` and their hashes in `scripts/release.py`
   (`licenses/README.md` says how).
5. **Approve** (the maintainer), within 30 days, as long as the built files
   are kept: the publishing job waits for the `release` environment's
   approval. It then creates a **draft** pre-release whose notes are the
   tag's `CHANGELOG.md` section, joined into one line per paragraph or list
   item (GitHub shows every newline of a release body as a line break, so
   the file's 80-column wrapping is undone there; `release.py notes`),
   followed by the pull requests merged since the previous release tag,
   which GitHub lists by label (`.github/release.yml`).
6. **Publish** (the maintainer): read the draft, publish it on the releases
   page, and announce it in the repository's Discussions.

Users check a release with `sha256sum -c SHA256SUMS` and
`gh attestation verify` naming the release workflow and the tag
(`--signer-workflow`, `--source-ref`, `--deny-self-hosted-runners`; the
[installation guide](docs/book/src/install.md) has the command).

A pull request that changes how a release is made (the workflow, its
scripts and tests, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
`about.toml`, `deny.toml`, `licenses/`) runs the build path as a dry run
(tests, build, check, licence notices, package, checksums; no attestation,
no release), and so does a manual run (`workflow_dispatch`) on any branch.
Editing a file the tarball only copies (`README.md`, `NOTICE`, the `LICENSE`
files) does not start it (ADR-0095). `about.toml`'s accepted licences must
stay the set `deny.toml` allows; `just test-release` compares them.

## How decisions are made

- **Why** something is the way it is lives in an Architecture Decision Record
  under [`docs/adr/`](docs/adr/) (MADR format; see its README). A new
  decision, or a change of mind, is a new ADR in a pull request with status
  `proposed`; the maintainer accepts it. Accepted ADRs are never rewritten,
  only superseded.
- **How** it works now lives in [`docs/design/`](docs/design/), updated with
  the code.
- **Where** it is in the code lives in [`ARCHITECTURE.md`](ARCHITECTURE.md).
- [`docs/research/`](docs/research/) holds the survey and experiments that
  preceded the code. It is frozen; new measurements go into a new experiment
  report or an ADR's evidence.

## Porting code from kaniko

ranikust reimplements kaniko's behaviour; it is not a line-by-line port.
Reading kaniko's source to learn what it does, and citing it as
`kaniko@<commit> path:line`, needs nothing special.

kaniko is licensed under the Apache License 2.0. When you translate kaniko
code closely enough that the Rust is a derivative of it (the same structure,
comments, tables or algorithms written the same way), you must:

1. Put a header at the top of each such file, above the module docs:

   ```rust
   // Derived from kaniko, https://github.com/osscontainertools/kaniko,
   // pkg/util/fs_util.go at commit 34ba9eadf.
   // Copyright 2018 Google LLC. Licensed under the Apache License, Version 2.0.
   // Modified for ranikust: <what changed, in one line>.
   ```

   Copy the copyright line from the kaniko file you ported.
2. Add an entry for the file under "Code derived from kaniko" in
   [`NOTICE`](NOTICE): the ranikust path, the kaniko path and the commit.
3. Name the kaniko file and commit in the commit message.

The parts derived from kaniko stay under Apache-2.0 terms; the header and the
NOTICE entry let anyone using the file comply with them. The same applies to
code from kaniko's dependencies under `vendor/`, each under its own license:
for example, porting `moby/patternmatcher` means also reproducing its NOTICE
text. Code under licenses other than those allowed in [`deny.toml`](deny.toml)
cannot be ported.

## AI-assisted contributions

AI coding tools are welcome; the repository ships instructions for them
([`CLAUDE.md`](CLAUDE.md), [`AGENTS.md`](AGENTS.md),
[`docs/agents/`](docs/agents/README.md), `.claude/`). The rules:

- **You are responsible for what you submit.** Read, understand and test
  every line, whoever or whatever wrote it. "The AI wrote it" is not an
  answer in review.
- **Disclose it.** The pull request template asks which tools you used and
  for what. Keep `Co-authored-by` trailers truthful.
- **No unattended contributions.** Agents do not open issues or pull
  requests, or reply in reviews, on their own. The maintainer's agents work
  within the limits of
  [ADR-0087](docs/adr/0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
  as amended by
  [ADR-0093](docs/adr/0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md)
  and label what they open `agent`.
- **Write the description yourself**, for a reader who never saw your
  session. Do not paste conversation logs.
- **Respect licenses.** Generated code that reproduces other projects' code
  follows the same attribution rules as any other port.
- Propose changes to `CLAUDE.md` or `docs/agents/rules.md` in the pull
  request description, under "Suggested agent rule additions", instead of
  editing them as a side effect of other work.

## License

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in ranikust by you, as defined in the Apache-2.0
license, shall be dual licensed under the MIT license and the Apache License
2.0, without any additional terms or conditions.
