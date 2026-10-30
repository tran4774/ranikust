# Contributing to ranikust

Thank you for your interest. ranikust is pre-alpha: the workspace exists, the
features do not yet. Before starting anything larger than a small fix, open
an issue so the work fits the plan in [`ARCHITECTURE.md`](ARCHITECTURE.md) and
the decisions in [`docs/adr/`](docs/adr/).

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
Security problems go through [`SECURITY.md`](SECURITY.md), never a public
issue.

## Development setup

ranikust builds and runs on Linux only. x86_64 is the release target; aarch64
must keep compiling ([ADR-0009](docs/adr/0009-x86-64-first-aarch64-ready.md)).
[docs/development-setup.md](docs/development-setup.md) walks through setting
up a machine step by step; the table below is the reference.

| Tool | Why | Install |
|---|---|---|
| [rustup](https://rustup.rs/) | Reads [`rust-toolchain.toml`](rust-toolchain.toml) and installs the pinned compiler, `rustfmt`, `clippy`, `rust-analyzer` and both musl targets on the first `cargo` command | your distribution, or rustup.rs |
| [just](https://github.com/casey/just) | Runs the recipes in [`justfile`](justfile) | `cargo install --locked just` |
| [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) | License, advisory and source checks | `cargo install --locked cargo-deny@0.20.2` |
| [mdBook](https://github.com/rust-lang/mdBook) | Builds the user guide in `docs/book` | `cargo install --locked mdbook@0.5.4` |
| [git-cliff](https://git-cliff.org/) | Drafts release notes; only needed by whoever prepares a release | `cargo install --locked git-cliff` |
| `file`, `readelf` | `just build-static` and `just test-aarch64` check the binary is static | `file` and `binutils` packages |
| A C compiler as `cc` | Builds the C dependencies (libzstd, mimalloc, ring) for the host, and links the static binary | `gcc` or `clang` package |
| [zig](https://ziglang.org/) 0.16.0 | Compiles the C dependencies for the musl targets in `just build-static`, `just check-aarch64` and `just test-aarch64` ([ADR-0020](docs/adr/0020-static-musl-via-cargo-zigbuild.md)) | see below |
| [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) | Hands zig to those builds | `cargo install --locked cargo-zigbuild@0.24.0` |
| `aarch64-linux-gnu-gcc` | Links the aarch64 binary as static-pie with its symbol table in `just test-aarch64` on x86_64 (on aarch64 the native `cc` does); without it the recipe links with zig and cannot check static-pie (Q-140) | `gcc-aarch64-linux-gnu` package |
| `qemu-aarch64` (QEMU user mode) 9.1 or later | `just test-aarch64`, so `just ci`, on an x86_64 machine: the aarch64 tests under emulation (8.2 has no `openat2`); a binfmt_misc entry also runs ranikust's own CLI tests. Not needed on aarch64, where they run natively (as in CI) | `qemu-user` package if 9.1 or later, else Debian 13's `qemu-user` unpacked with `dpkg -x`; `QEMU_AARCH64=<path>` |
| rootless [Podman](https://podman.io/) | Only for `just smoke`, the T1 smoke test ([docs/testing.md](docs/testing.md)), `just test-vm e2e`, which saves the reference images from it, and `just registry-up` (`podman compose`, with `openssl`, python3's `bcrypt` and `skopeo`) | `podman`, `podman-compose` or `docker-compose`, `python3-bcrypt`, `skopeo` packages |
| A [`nextjs-kaniko`](docs/testing.md#end-to-end-workloads-e2e) checkout next to the repository, with its registry and verdaccio storage filled and its `kaniko-bench:stream`, BuildKit, `registry:2` and verdaccio images in podman | Only for `just test-vm e2e`; read, never written | `RANIKUST_NEXTJS_KANIKO=<path>` |
| QEMU with KVM, `xorriso`, `jq` | Only for `just test-vm`, the T2 VM harness; `/dev/kvm` must be readable and writable by you (its group or an ACL), never through `sudo` | `qemu-system-x86`, `qemu-utils`, `xorriso`, `jq` packages |

The versions of cargo-deny, mdBook, zig and cargo-zigbuild match CI. Nothing
in the build needs root, and nothing in the dev loop should ever be run with
`sudo`.

zig is one tarball; install it into your home and check it against Zig's
signing key (from [ziglang.org/download](https://ziglang.org/download/)):

```sh
v=0.16.0 a=x86_64                     # or a=aarch64
f=zig-$a-linux-$v.tar.xz
curl -fLO https://ziglang.org/download/$v/$f
curl -fLO https://ziglang.org/download/$v/$f.minisig
minisign -Vm "$f" -P RWSGOq2NVecA2UPNdBUZykf1CCb147pkmdtYxgb3Ti+JO/wCYvhbAb/U
mkdir -p ~/.local/share/zig ~/.local/bin
tar -C ~/.local/share/zig -xf "$f"
ln -sf ~/.local/share/zig/zig-$a-linux-$v/zig ~/.local/bin/zig
zig version
```

Without `minisign`, compare `sha256sum "$f"` with the tarball's `shasum` in
<https://ziglang.org/download/index.json>.

## Development loop

```sh
just                      # list the recipes
cargo test -p rnk-rootfs   # narrow: the crate you changed (add a test name filter)
just check                # fmt check + clippy -D warnings + all T0 tests
just ci                   # everything CI runs, before you open a pull request
```

Also run `just check-aarch64` after touching syscalls or `cfg`, `just deny`
after changing dependencies, and `just doc` / `just book` after changing
documentation. Every recipe that mirrors CI runs exactly the command of the
CI job; if you change one, change the other.

[`docs/agents/rules.md`](docs/agents/rules.md) lists the traps the lints do
not catch and the host-safety rules. Read it even if you do not use an AI agent: it is the
short version of how to write code here.

## Tests

| Tier | What | Privileges | Where | Status |
|---|---|---|---|---|
| T0 | Unit and in-process tests | None | Anywhere: `just test` | In use |
| T1 | Rootless integration: user namespaces, rootless overlay, Podman | A regular user with subordinate IDs | Your machine | Planned |
| T2 | Privileged integration: real root, overlay without user namespaces, Docker rootful, the host-safety attack suite | root | A disposable VM only: `just test-vm` | In use |
| T3 | CI: format, clippy, T0, static build, aarch64 check, cargo-deny, docs | None | GitHub Actions on every pull request | In use |

Tests above T0 are marked `#[ignore = "T1: ..."]` or `#[ignore = "T2: ..."]`
and run only through their harness. **Never run a T2 test, or ranikust in
kaniko mode, on your own machine**: kaniko mode empties its root filesystem.
T2 tests run in a VM that `just test-vm` boots and deletes;
[`docs/testing.md`](docs/testing.md) has the details.

A bug fix comes with a test that fails without the fix.

## Commits and pull requests

Start every change from the latest `main` on its own branch, named
`<type>/<scope>-<topic>` (for example `feat/rnk-oci-types`):

```sh
git switch main && git pull --ff-only && git switch -c feat/rnk-oci-types
```

Commit at each checkpoint, a step that builds and passes `just check` with
its documentation already updated. Updating the affected documentation is
part of every change, not a follow-up. Questions the design does not answer
go to [`docs/open-questions.md`](docs/open-questions.md) instead of being
guessed.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/);
[git-cliff](cliff.toml) groups them into release notes.

```
<type>(<scope>): <summary in the imperative, lower case, no full stop>

<body: what and why, wrapped at 72 columns>

<footers: BREAKING CHANGE: ..., Refs: #123>
```

| Type | Use for | In release notes |
|---|---|---|
| `feat` | New user-visible behaviour | Added |
| `fix` | Bug fix | Fixed |
| `perf` | Faster, same behaviour | Changed |
| `refactor`, `test`, `docs`, `ci`, `build`, `chore`, `style` | Everything else | Not listed |

The scope is the crate name (`rnk-rootfs`, `ranikust`) or an area (`ci`,
`book`, `adr`). Mark breaking changes with `!` after the type or a
`BREAKING CHANGE:` footer; use the `security` scope for security fixes.
Summaries starting with "deprecate" or "remove" are listed under Deprecated
and Removed.

Pull requests are squash-merged, so the pull request title becomes the
commit message and must follow the same format. The
[pull request template](.github/PULL_REQUEST_TEMPLATE.md) asks for a summary,
a test plan, the docs you updated and whether AI tools helped. A pull request
keeps the documentation in sync in the same change, following the "Keep in
sync" list in [`docs/agents/rules.md`](docs/agents/rules.md); user-visible changes get an entry
under `Unreleased` in [`CHANGELOG.md`](CHANGELOG.md).

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
([`CLAUDE.md`](CLAUDE.md), [`AGENTS.md`](AGENTS.md), [`docs/agents/`](docs/agents/README.md),
`.claude/`). The rules:

- **You are responsible for what you submit.** Read, understand and test
  every line, whoever or whatever wrote it. "The AI wrote it" is not an
  answer in review.
- **Disclose it.** The pull request template asks which tools you used and
  for what. Keep `Co-authored-by` trailers truthful.
- **No unattended contributions.** Agents do not open issues or pull
  requests, or reply in reviews, on their own.
- **Write the description yourself**, for a reader who never saw your
  session. Do not paste conversation logs.
- **Respect licenses.** Generated code that reproduces other projects' code
  follows the same attribution rules as any other port.
- Propose changes to `CLAUDE.md` or `docs/agents/rules.md` in the pull
  request description, under "Suggested agent rule additions", instead of editing them
  as a side effect of other work.

## License

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in ranikust by you, as defined in the Apache-2.0
license, shall be dual licensed under the MIT license and the Apache License
2.0, without any additional terms or conditions.
