# Experiment 10: EXDEV and package managers

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | With `redirect_dir=nofollow`, renaming a directory that lives on a lower layer fails with `EXDEV`; do apt/dpkg, pip, yarn and cargo still produce correct results when a later `RUN` changes what an earlier one installed? |
| **Answer** | Yes. All nine package-manager scenarios gave output identical to a run without overlay. Only pip actually hit `EXDEV` here (6 renames per scenario) and it fell back to copying; dpkg, apt, yarn and cargo never rename lower directories. |
| **Related** | [ADR-0016](../../adr/0016-overlay-mount-options.md), [overlay snapshot design](../../design/overlay-snapshot.md), experiments [01](01-overlay-semantics.md), [06a](06a-root-rerun.md), [11](11-root-extras.md) |
| **Original** | `docs-rust/poc/10-exdev-pkg-managers/` (Vietnamese, not in this repository) |

## Summary

- Extends [experiment 01](01-overlay-semantics.md) §3.2 (npm, pnpm) and the
  "`rename(2)` of a lower directory returns `EXDEV`" row of
  [survey 06](../survey/06-overlay-and-parallelism.md) §3.4 to apt/dpkg, pip
  (venv and system), yarn classic, yarn berry (node-modules and PnP) and
  cargo.
- Each scenario simulated two overlay-mode `RUN`s with the recommended mount
  options, counted renames returning `EXDEV` under `strace`, and compared the
  final state with a control run on a plain rootfs.
- **Every scenario was correct**, identical to the control **[measured]**.
- Only pip hit `EXDEV` (6 directory renames in `RUN#2`, in both venv and
  system installs); it falls back to copy + delete. The positive control
  (`mv` of a directory from `RUN#1`) showed the harness catches `EXDEV`
  **[measured]**.
- No upper contained an `*.overlay.redirect` xattr: `redirect_dir=nofollow`
  creates no redirects **[measured]**.

## Questions

1. Do apt/dpkg, pip, yarn (classic and berry) and cargo give correct results
   when a `RUN` modifies or removes what an earlier `RUN` installed, under
   overlay with `redirect_dir=nofollow`?
2. Which of them rename lower directories and therefore hit `EXDEV`?
3. Does `redirect_dir=nofollow` ever create redirect xattrs?

## Environment

- **Date:** 2026-10-29.
- **Host:** kernel 7.0.0-34, podman 5.7 rootless.
- **Privilege:** `podman run --privileged --cpus=4`, so overlay is the overlay
  inside a user namespace.
- **Filesystems:** the container's rootfs as the base (lowest lower); uppers
  and workdirs on a tmpfs at `/t`.
- **Network:** needed for apt (deb.debian.org), PyPI, the npm registry and
  crates.io. `nk-verdaccio` and `nk-registry` were **not** used.
- **Images and tool versions:** see the results table.

## Method

Same approach as experiment 01's npm/pnpm test: simulate two `RUN`s of
ranikust's overlay mode.

1. **SETUP** runs directly on the container's rootfs: install `strace` and
   `attr`, create test packages. The result is the "base image", i.e. the
   lower of RUN#1.
2. **RUN#k:**
   - mount overlay with `lowerdir=u(k-1):…:u1:/` and a new upper and work on
     tmpfs `/t`;
   - recommended options
     `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile`;
   - mount `proc`, rbind `/dev`, bind `resolv.conf`;
   - `chroot`, run the command under
     `strace -f -e trace=rename,renameat,renameat2`, and count renames that
     return `EXDEV`.
3. **VERIFY** runs in the merged rootfs after RUN#2: installed versions, the
   package's directory tree, and the tool's own checks (`dpkg -V`,
   `dpkg --audit`, `pip check`, `require()`).
4. **Control:** SETUP, CMD1, CMD2 and VERIFY run again directly on the rootfs,
   **without overlay**, in a fresh container. **"Correct" means: the commands
   return 0 and the VERIFY output is identical to the control's.**

The `control-mv` scenario is the positive control: `mv` of a directory created
by RUN#1. It shows that the harness catches `EXDEV`.

## Results

All results are **[measured]**.

"EXDEV in RUN#2" gives the renames that returned `EXDEV` out of all renames
traced in RUN#2.

| Scenario | Image, version | RUN#1 | RUN#2 (the directory to change is on a lower layer) | `EXDEV` in RUN#2 | Result |
|---|---|---|---|---|---|
| `control-mv` | debian, coreutils 9.7 | `mkdir -p /d/sub` | `mv /d /d2` | **1** (`renameat2(... RENAME_NOREPLACE)`) | **Correct**: `mv` copies by itself |
| `dpkg-upgrade` | debian, dpkg 1.22.22 | `dpkg -i pkga_1.0.deb` (with `/opt/pkga/lib/{sub,old}`) | `dpkg -i pkga_2.0.deb` (drops `lib/old`, adds `lib/new`, changes files) | 0 / 19 | **Correct**: `dpkg -V` OK, tree identical to the control |
| `dpkg-purge` | as above | `dpkg -i pkga_1.0.deb` | `dpkg -P pkga` | 0 / 10 | **Correct**: `/opt/pkga` and `/usr/share/doc/pkga` gone, no info files left |
| `apt` | debian, apt 3.0.3 | `apt-get update && apt-get install curl` | `apt-get purge curl && autoremove && update && install jq && clean` | 0 / 285 | **Correct**: curl and libcurl4t64 fully removed, jq 1.7.1 runs, `dpkg --audit` OK |
| `pip-venv` | debian, Python 3.13.5, pip 25.1.1 | `python3 -m venv /venv`; `pip install idna==3.6 requests==2.31.0` | `pip install -U idna==3.7 requests==2.32.3`; `pip uninstall certifi` | **6** / 36 | **Correct**: pip switches to copying, result identical to the control |
| `pip-system` | as above, `--break-system-packages` into `/usr/local/lib/python3.13/dist-packages` | as above, without a venv | as above | **6** / 46 | **Correct** |
| `yarn-classic` | node:22-bookworm-slim, node 22.23.4, yarn 1.22.22 | `yarn add file:pkga-1.0.0.tgz lodash@4.17.20 is-number@7.0.0` | `yarn add file:pkga-2.0.0.tgz lodash@4.17.21`; `yarn remove is-number` | 0 / **0** | **Correct**: `require` gives `2.0.0 4.17.21`, `is-number` gone |
| `yarn-berry-nm` | yarn 4.9.2 (corepack), `nodeLinker: node-modules` | as above | as above | 0 / 4 | **Correct** |
| `yarn-berry-pnp` | yarn 4.9.2, PnP (default) | as above | as above | 0 / 6 | **Correct** |
| `cargo` | rust:1-bookworm, cargo 1.99.1 | `cargo new`, add `itoa@=1.0.10`, `cargo build` (debug, incremental), `cargo install --path .` | move to `itoa@=1.0.11`, add `cfg-if`, change code and version, `cargo build`, `cargo install` over the old one | 0 / 16 | **Correct**: the newly built and installed binary prints `Hi v2`, `.crates.toml` records `app 0.2.0`, the registry holds all 3 crates |

In every scenario, the `*.overlay.redirect` xattr count in the uppers was 0,
so `redirect_dir=nofollow` creates no redirects. No rename in RUN#1 returned
`EXDEV` in any scenario.

The renames that returned `EXDEV` for pip (RUN#2, venv):

```
rename("/venv/lib/python3.13/site-packages/idna-3.6.dist-info/", ".../~dna-3.6.dist-info") = -1 EXDEV
rename("/venv/lib/python3.13/site-packages/idna/", ".../~dna") = -1 EXDEV
rename(".../requests-2.31.0.dist-info/", ".../~equests-2.31.0.dist-info") = -1 EXDEV
rename(".../requests/", ".../~equests") = -1 EXDEV
rename(".../certifi-2026.10.23.dist-info/", ".../~ertifi-2026.10.23.dist-info") = -1 EXDEV
rename(".../certifi/", ".../~ertifi") = -1 EXDEV
```

pip "stashes" the directories of the old version by renaming them to `~xxx`
before uninstalling. On `EXDEV` it falls back to copy + delete, and the result
is correct.

## Findings

1. `EXDEV` handling gave correct results in **every** tool tested: npm 10 and
   pnpm 9 ([experiment 01](01-overlay-semantics.md)), dpkg 1.22, apt 3.0,
   pip 25.1, yarn 1.22, yarn 4.9 (node-modules and PnP) and cargo 1.99. Only
   npm and pip actually hit `EXDEV`, and both fall back to copying
   **[measured]** (results table).
2. dpkg and apt do not rename directories. dpkg renames **files**
   (`.dpkg-new` → the real name), and renaming a lower file works (copy-up).
   This matches the reasoning in experiment 01 **[measured]**.
3. yarn classic calls no `rename` at all: it copies from its cache into
   `node_modules`. yarn berry and cargo only rename files, or directories just
   created in the upper; for example rustc's incremental session directory
   `s-…-working` is created and then renamed while it is in the upper, so it
   does not fail **[measured]**.
4. The price of `EXDEV` is copying the whole tree instead of renaming it. For
   pip the package directories are small, so it is negligible. Large packages
   (for example upgrading a multi-GB `torch` in a venv on a lower layer) were
   **not measured**; they would be clearly slower and use extra upper space
   while running **[inferred]**.

## Implications for ranikust

- The `EXDEV` that `redirect_dir=nofollow` causes on lower-directory renames is
  accepted as part of the overlay mount options
  ([ADR-0016](../../adr/0016-overlay-mount-options.md),
  [overlay snapshot design](../../design/overlay-snapshot.md)).
- The line "yarn, pip, cargo and dpkg not yet tested" in
  [survey 06](../survey/06-overlay-and-parallelism.md) §3.4 and in the
  [survey summary](../survey/00-summary.md) §4 can read: tested, correct.

## Limitations and open questions

- Each tool was tried with only one or two short scenarios. Not tried:
  `apt-get dist-upgrade` of a real package whose directory layout changes
  (needs two versions of the same package in the mirror), `pip install -e`,
  `poetry`, `uv`, `conda`, `gem`, `composer`, `go mod download` (`go` renames
  temporary directories into the module cache; they are freshly created, so it
  is expected not to fail **[inferred]**).
- Run in a rootless privileged container. As real root,
  [experiment 06a](06a-root-rerun.md) showed that renaming a lower directory
  still returns `EXDEV` even with `redirect_dir=off`, so the result should not
  change **[inferred]**.
- The cost of the copy fallback for large directories is not measured
  (Finding 4).

## Reproduction

The PoC sources are archived outside this repository, under
`docs-rust/poc/10-exdev-pkg-managers/`:

- `cases/<scenario>.sh`: one file per scenario, defining the image, `SETUP`,
  `CMD1`, `CMD2` and `VERIFY`.
- `in-container.sh <overlay|plain> <case-file>`: runs inside
  `podman run --privileged`; in `overlay` mode it mounts each `RUN` as
  described under Method and counts `EXDEV` renames, in `plain` mode it runs
  the commands directly as the control.
- `run.sh [scenario...]`: for each scenario runs both modes in fresh
  containers (`--privileged --cpus=4 --tmpfs /t`), writes `out/<scenario>.txt`
  and appends a final line `VERIFY overlay vs plain: same|different` (the
  words are Vietnamese in the original output). All ten outputs end with
  "same".

```sh
cd docs-rust/poc/10-exdev-pkg-managers
./run.sh                 # every scenario in cases/; or ./run.sh apt cargo
```

## References

1. [Experiment 01: Overlay semantics](01-overlay-semantics.md) §3.2 (npm,
   pnpm).
2. [Experiment 06a: Root re-run](06a-root-rerun.md).
3. [Survey 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md)
   §3.4.
4. [Survey 00: Summary](../survey/00-summary.md) §4.
