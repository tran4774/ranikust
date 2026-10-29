# Experiment 06a: Root re-run

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Do the results of experiments 01 (overlay semantics), 02 (`RUN` sandbox) and 06 (overlay vs walk) hold when run as real root in the initial user namespace, directly on the host? |
| **Answer** | Yes. Real root adds rights (`redirect_dir=off` without `userxattr`, `trusted.*` xattrs, a fresh `proc` mount) but the recommended option set works in every mode. A bare `chroot` escapes to the host's `/` even with dropped capabilities. Overlay diffing was 3.9× (ext4) and 5.2× (tmpfs) faster than the walk; whole sequence −19 % on ext4 and −32 % on tmpfs. |
| **Related** | [ADR-0016](../../adr/0016-overlay-mount-options.md), [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md), [sandbox design](../../design/sandbox.md), [overlay snapshot design](../../design/overlay-snapshot.md), experiments [01](01-overlay-semantics.md), [02](02-run-sandbox.md), [06](06-overlay-vs-walk.md), [11](11-root-extras.md) |
| **Original** | `docs-rust/poc/root-run/` (Vietnamese, not in this repository) |

## Summary

- Experiments 01, 02 and 06 originally ran in rootless podman, where root
  lives in a user namespace. This experiment repeated the same tests as root in
  the initial user namespace, on the host, without a container: the "outside a
  container, with root" path of
  [survey 06](../survey/06-overlay-and-parallelism.md) §1.
- **Overlay:** the recommended option set
  `userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off,uuid=off,volatile`
  works as real root too, so one option set serves every mode. Whiteouts,
  copy-up, `EXDEV` on directory renames and the 500-layer limit behave exactly
  as in a user namespace **[measured]**.
- **Sandbox:** every step works without a fallback. A bare `chroot` escapes to
  the host's real `/`, even with `--drop-caps`; `pivot_root` and MS_MOVE +
  `chroot` do not. This is the strongest evidence that `pivot_root` is
  mandatory **[measured]**.
- **Overlay vs walk:** diffing was 3.9× faster on ext4 and 5.2× on tmpfs;
  commands were 21 % (ext4) and 9 % (tmpfs) slower; the whole sequence was
  19 % and 32 % faster. Without `volatile`, `churn-edit` took 13.0 s, as in the
  rootless run **[measured]**.
- Running as root and then creating a user namespace (`--userns`) requires
  every parent directory of the workdir to be traversable by the mapped uid
  **[measured]**.

## Questions

1. Does real root change which overlay options are accepted, and how upper
   directories behave (experiment 01)?
2. Does the `RUN` sandbox need any fallback as real root, and which ways of
   entering the rootfs can be escaped (experiment 02)?
3. Do the overlay-vs-walk timings and file sets of experiment 06 hold as real
   root?

## Environment

- **Date:** 2026-10-29; the 01 and 02 parts at 07:48–07:49, the 06 timings
  from 07:49 to 07:58.
- **Host:** the survey machine (see the [research README](../README.md)),
  kernel 7.0.0-34-generic.
- **Privilege:** real root in the initial user namespace, run directly on the
  host via `pkexec`: uid 0, `uid_map` `0 0 4294967295`, `CapEff` and `CapBnd`
  `000001ffffffffff` (all capabilities), AppArmor `unconfined`, no seccomp
  (`Seccomp: 0`).
- **Isolation of the test itself:** the whole script ran in a private mount
  namespace (`unshare -m --propagation private`). `/tmp` was a 14 GB tmpfs and
  `/etc/hosts` a bind mount, both visible only in that namespace.
- **Filesystems:** `/tmp` tmpfs; workdir on ext4 (`/home`, `/dev/nvme0n1p4`).
- **Tools in the npm/pnpm rootfs:** node v22.13.1, npm 10.9.2, pnpm 9.15.9,
  strace 6.12.
- **Load during the 06 part:** 1-minute load average 1.29–5.65
  (`out/06-run.log`).
- **CPU limit for the 4-CPU set:** CPU affinity (`taskset -c 0-3`), not a
  cgroup quota as in the rootless run (`--cpus=4`).

## Method

**Preparation, as a normal user:**

- `stage/bin/sandbox` and `stage/bin/ovpoc`: static builds of the
  experiment 02 and experiment 06 binaries;
- `stage/tars/{alpine,debian,node,verdaccio}.tar`: root filesystems made with
  `podman export`.

The 06 part also needs `nk-registry` (127.0.0.1:5001) and `nk-verdaccio`
(127.0.0.1:4873) running.

**Safety measures of the script:**

- Mounts: the whole script runs in its own mount namespace, so every mount
  disappears when it ends.
- `/tmp` and `/etc/hosts` are only shadowed inside that namespace (tmpfs and
  bind mount); the real files are not written.
- Temporary data lives in `stage/work/` and is deleted at the end; results are
  handed back to the invoking user.
- Caveat: the `parentdeath` test of experiment 02 runs
  `pkill -9 -f 'sleep 1000'`, which kills every `sleep 1000` process on the
  machine.

**Tests:**

- **01, overlay:** experiment 01's privilege probe on tmpfs and ext4, `proc`
  mounts in new PID and user namespaces, the upper-directory semantics script
  in seven configurations (A–G: default `trusted.*`, `userxattr`,
  `redirect_dir=off`, the recommended set, the recommended set in a new user
  namespace, and two ext4 configurations with `volatile`), the mount-option
  matrix as real root and as root + `unshare -Urm`, lowerdir limits, xattrs in
  layer content, and npm/pnpm `EXDEV` behaviour with the package directory on
  the lower layer.
- **02, sandbox:** experiment 02's full test script, plus the sandbox inside a
  new user namespace (`unshare -Ur`) to emulate a process without
  `CAP_SYS_ADMIN`.
- **06, overlay vs walk:** the same `ovpoc` binary and steps as
  [experiment 06](06-overlay-vs-walk.md), base image from `nk-registry`. Three
  sets, 3 rounds each with configurations interleaved:

  | Set | Configurations | CPU |
  |---|---|---|
  | `root-ext4` | `ov`, `ovv`, `hwh-32`, `xxh-32`, `hwh-4` | 32 |
  | `root-tmpfs` | `ov`, `hwh-32` | 32 |
  | `root-cpus4-ext4` | `ovv`, `hwh-4` | `taskset -c 0-3` |

  The file sets were compared as in experiment 06
  (`06-overlay-vs-walk/results/root-cmp-*.txt`).

## Results

All values are **[measured]**.

### 01: overlay

| Check | Real root | Compared with root in a userns (experiment 01) |
|---|---|---|
| Mount overlay on tmpfs and ext4, with or without `userxattr` | OK | Same |
| `redirect_dir=off` | **OK without `userxattr`.** With `userxattr` still `EINVAL`, because `userxattr` forces `nofollow` | Always `EINVAL` in a userns |
| Recommended set `userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off,uuid=off,volatile` | **OK** | OK. So one option set works for every mode |
| `userxattr` not passed | xattrs are `trusted.overlay.*` (`origin`, `impure`, `uuid`, `opaque`) | The kernel enables `userxattr` itself |
| Whiteouts share one inode (`nlink=4`); copy-up breaks hardlinks; `rename(2)` of a lower directory returns `EXDEV` (even with `redirect_dir=off`); `chmod`/`touch` copy up the whole file | Identical | Same |
| New `proc` in a new userns + new pidns | **OK** (the host's `/proc` is not masked) | `EPERM` in a container, because `/proc` is masked |
| `mknod c 0 0`, `setxattr trusted.overlay.opaque` | OK | `trusted.*` → `EPERM` |
| `user.overlay.foo` xattr already present in layer content | `trusted` mount: **kept**, not hidden | `userxattr` mount: hidden, and lost on copy-up |
| lowerdir limit | Same: 500 layers; `mount(2)` truncates at about 4 KB and returns `ENOENT`; `fsconfig("lowerdir")` returns `EINVAL`; `lowerdir+` OK up to 500 | Same |
| npm, pnpm with the package directory on the lower layer | npm hits `EXDEV` twice, falls back to copying, result correct; pnpm does not rename | Same |

The "root + `unshare -Ur`" tests on ext4 failed with `Permission denied`.
This comes from the test environment, not from overlay: `/home/tran` cannot
be traversed by uids outside the map, and inside a new user namespace root no
longer bypasses permission checks on inodes whose owner is not mapped.
Consequence for the design: when ranikust runs as root and creates its own
user namespace (the `--userns` option), every parent directory of the workdir
must be traversable by the mapped uid.

### 02: sandbox

- **Every step works without a fallback:** `mknod /dev/null`, read-only
  `sysfs`, `pivot_root`, `sethostname`. `apk add`, `apt-get install curl` and
  `npm --version` ran, as did `USER nobody/node/_apt/1234`.
- **A bare `chroot` escapes to the host's real `/`:** the escaped process saw
  `boot cdrom lost+found snap swap.img …`. It still escapes with
  `--drop-caps`, because Docker's capability set keeps `CAP_SYS_CHROOT`.
  `pivot_root` and MS_MOVE + `chroot` cannot be escaped this way. This is the
  strongest evidence that **`pivot_root` is mandatory**.
- **Without dropping capabilities, `RUN` has full power:** mounting a tmpfs,
  unmounting `/etc/resolv.conf` and then writing straight into the rootfs all
  succeed. With `--drop-caps`, all three are blocked.
- **PDEATHSIG:** with it, no `sleep 1000` is left behind; without it, one
  process survives. The `resolv.conf` stub is still left behind after a
  SIGKILL.
- **Overhead per `RUN`** (`/bin/true` × 200, mean):

  | Variant | Time per run |
  |---|---|
  | baseline (no sandbox) | 0.11 ms |
  | full sandbox | 1.05 ms |
  | `chroot` | 0.98 ms |
  | `+drop-caps` | 1.06 ms |
  | `+init` | 1.09 ms |

### 06: overlay vs walk (medians of 3 rounds, s)

| | Diff, 5 steps | Commands | `syncfs` | **Total** |
|---|---|---|---|---|
| ext4, walk `hwh-32` | 2.58 | 3.22 | 0.70 | **6.50** |
| ext4, overlay `ovv` (volatile) | 0.66 | 3.91 | 0.69 | **5.26** (−19 %) |
| ext4, overlay `ov` (no volatile) | 0.69 | **16.69** | 0.04 | 17.42 |
| tmpfs, walk `hwh-32` | 2.46 | 2.97 | 0 | **5.43** |
| tmpfs, overlay `ov` | 0.47 | 3.23 | 0 | **3.70** (−32 %) |
| ext4, `taskset -c 0-3`, walk `hwh-4` | 2.92 | 4.26 | 0.71 | **7.89** |
| ext4, `taskset -c 0-3`, overlay `ovv` | 0.65 | 5.15 | 0.69 | **6.49** (−18 %) |

- Diffing was 3.9× faster on ext4 and 5.2× faster on tmpfs. Overlay made the
  commands slower: +21 % on ext4, +9 % on tmpfs.
- Without `volatile` it is as slow as in the rootless run: `churn-edit` took
  13.0 s.
- File sets (`06-overlay-vs-walk/results/root-cmp-*.txt`): identical to the
  rootless run, including the 1,813 hardlinked files in the pnpm store that
  only the walk reports.
- The 4-CPU rows used CPU affinity (`taskset`); the rootless run used a cgroup
  quota (`--cpus=4`), so the two are not directly comparable.

Ranges and per-step numbers are in the [appendix](#appendix-full-tables).

## Findings

1. One overlay option set
   (`userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off,uuid=off,volatile`)
   mounts as real root, as root in a fresh userns and as rootless root
   **[measured]** (01 table).
2. Real root accepts `redirect_dir=off` only without `userxattr`; directory
   renames of lower directories return `EXDEV` either way **[measured]**
   (01 table).
3. Overlay semantics that matter for diffing (whiteouts, copy-up, hardlink
   breaking, lowerdir limits, npm/pnpm behaviour) are identical as real root
   **[measured]** (01 table).
4. With a `trusted` mount, `user.overlay.*` xattrs in layer content are kept;
   with `userxattr` they are hidden and lost on copy-up **[measured]**
   (01 table).
5. A bare `chroot` sandbox can be escaped to the host's `/` by a root process
   that keeps `CAP_SYS_CHROOT`, even with Docker's reduced capability set;
   `pivot_root` cannot **[measured]** (02).
6. Without dropping capabilities, `RUN` can unmount the sandbox's stubs and
   write into the rootfs **[measured]** (02).
7. The sandbox costs about 1 ms per `RUN` over a 0.11 ms baseline
   **[measured]** (02 overhead table).
8. Overlay vs walk as real root reproduces experiment 06: diffing 3.9–5.2×
   faster, whole sequence 18–32 % faster (ext4 with `volatile`, or tmpfs), and
   `volatile` still required on ext4 **[measured]** (06 table).

## Implications for ranikust

- The overlay mount options are the same in every privilege mode, with
  `volatile` included and `EXDEV` on lower-directory renames accepted
  ([ADR-0016](../../adr/0016-overlay-mount-options.md),
  [overlay snapshot design](../../design/overlay-snapshot.md)).
- The `RUN` sandbox enters the rootfs with `pivot_root`, never with a bare
  `chroot`, and drops capabilities ([sandbox design](../../design/sandbox.md)).
- With the opt-in `--userns`
  ([ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md)),
  every parent of the workdir must be traversable by the mapped uid; the
  sandbox design has to check or document this.

## Limitations and open questions

- The "new user namespace" variants that needed scripts under `/home` did not
  run: configuration E of the semantics script and the layer-xattr test in
  `unshare -Urm` failed with `Permission denied` (the traversal issue described
  under 01). The option matrix in a new userns did run.
- Configurations F and G of the semantics script (ext4, with `volatile`) ended
  with `OSError: [Errno 22]` on the re-mount step. This was not noticed at the
  time; [experiment 11](11-root-extras.md) explains it: after a `volatile`
  mount, the same upper/work pair cannot be mounted again until
  `work/incompat/volatile` is removed **[measured]**.
- The 4-CPU set used `taskset`, the rootless run `--cpus=4`; compare within a
  run, not across runs.
- Three rounds per configuration, one shared machine, warm page cache.
- Only ext4 and tmpfs; xfs and btrfs were covered later in
  [experiment 11](11-root-extras.md).

## Reproduction

The script and its staged inputs are archived outside this repository, under
`docs-rust/poc/root-run/`:

- `run-as-root.sh [01] [02] [06]` (default: all three, about 20–30 minutes)
  re-executes itself in a private mount namespace, then calls the scripts of
  experiments 01 and 02 and the `ovpoc` binary of experiment 06.
- Outputs: `out/01-quyen.txt` … `out/07-giai-nen-layer.txt` (same names as in
  experiment 01's `out/`), `out/02-sandbox.txt`, `out/06-tables.md`,
  `out/06-run.log`, and `06-overlay-vs-walk/results/root-*`.

```sh
sudo bash docs-rust/poc/root-run/run-as-root.sh          # 01, 02 and 06 (about 20–30 minutes)
sudo bash docs-rust/poc/root-run/run-as-root.sh 01 02    # skip the performance part
```

The recorded run used `pkexec` instead of `sudo`; the script accepts either.

## References

1. [Experiment 01: Overlay semantics](01-overlay-semantics.md).
2. [Experiment 02: RUN sandbox](02-run-sandbox.md).
3. [Experiment 06: Overlay vs walk](06-overlay-vs-walk.md).
4. [Experiment 11: Root extras](11-root-extras.md) (`volatile` lifecycle).
5. [Survey 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md)
   §1 (mode selection).

## Appendix: full tables

Experiment 06 re-run as real root. Generated by experiment 06's
`summarize.py`. Medians with (min–max) over 3 rounds.

### `root-ext4`

Rounds per configuration: hwh-32=3, hwh-4=3, ov=3, ovv=3, xxh-32=3

Diff time (s): overlay = walk of the upperdir; walk = walk + lstat + hash + map
comparison. Excludes `syncfs`, tar and compression.

| configuration | init | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | **total** | total + syncfs |
|---|---|---|---|---|---|---|---|---|
| hwh-32 | 0.035 (0.033–0.036) | 0.032 (0.032–0.033) | 0.885 (0.873–0.890) | 0.864 (0.851–0.886) | 0.386 (0.373–0.390) | 0.402 (0.388–0.403) | **2.58 (2.58–2.63)** | 3.28 (3.22–3.33) |
| hwh-4 | 0.037 (0.037–0.038) | 0.037 (0.031–0.038) | 0.959 (0.946–0.996) | 1.012 (0.984–1.017) | 0.451 (0.445–0.462) | 0.495 (0.490–0.507) | **3.01 (2.95–3.02)** | 3.67 (3.65–3.72) |
| ov | – | 0.004 (0.004–0.004) | 0.606 (0.603–0.625) | 0.046 (0.044–0.048) | 0.032 (0.031–0.032) | 0.006 (0.005–0.006) | **0.69 (0.69–0.71)** | 0.73 (0.73–0.76) |
| ovv | – | 0.004 (0.004–0.004) | 0.577 (0.569–0.644) | 0.041 (0.039–0.047) | 0.030 (0.028–0.033) | 0.009 (0.007–0.010) | **0.66 (0.65–0.74)** | 1.38 (1.34–1.42) |
| xxh-32 | 0.029 (0.029–0.030) | 0.029 (0.028–0.030) | 0.871 (0.867–0.875) | 0.881 (0.879–0.911) | 0.382 (0.362–0.389) | 0.380 (0.379–0.397) | **2.57 (2.57–2.61)** | 3.28 (3.27–3.28) |

Command run time (s) and `syncfs` after each command (s):

| configuration | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | commands total | syncfs total |
|---|---|---|---|---|---|---|---|
| hwh-32 | 0.77 (0.74–0.77) | 1.93 (1.90–1.99) | 0.25 (0.25–0.25) | 0.22 (0.22–0.22) | 0.05 (0.05–0.05) | 3.22 (3.18–3.24) | 0.70 (0.65–0.70) |
| hwh-4 | 0.77 (0.77–0.78) | 1.96 (1.91–1.96) | 0.25 (0.25–0.25) | 0.21 (0.21–0.22) | 0.05 (0.04–0.05) | 3.24 (3.20–3.24) | 0.70 (0.65–0.71) |
| ov | 0.81 (0.80–0.83) | 2.32 (2.32–2.43) | 0.47 (0.46–0.47) | 13.02 (12.94–13.04) | 0.07 (0.07–0.07) | 16.69 (16.60–16.81) | 0.04 (0.04–0.04) |
| ovv | 0.78 (0.75–0.82) | 2.22 (2.13–2.30) | 0.43 (0.42–0.46) | 0.42 (0.41–0.44) | 0.07 (0.07–0.07) | 3.91 (3.78–4.09) | 0.69 (0.68–0.73) |
| xxh-32 | 0.78 (0.74–0.78) | 1.89 (1.84–1.89) | 0.26 (0.25–0.26) | 0.22 (0.22–0.23) | 0.05 (0.04–0.05) | 3.17 (3.15–3.19) | 0.70 (0.67–0.71) |

Diff size and export cost (medians): file = files + symlinks; tar and gz in MB;
tar s = tar to /dev/null; tgz s = tar + pgzip with 4 threads to /dev/null.

| configuration | step | walked | file | dir | whiteout | tar MB | gz MB | tar s | tgz s | hash read MB | cache hits |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hwh-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 249 | 0 |
| hwh-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.12 (1.12–1.16) | 1.96 (1.94–1.97) | 2432 | 0 |
| hwh-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1883 | 7404 |
| hwh-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 344 | 215499 |
| hwh-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.02) | 0.03 (0.03–0.03) | 344 | 215499 |
| hwh-4 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 249 | 0 |
| hwh-4 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.11 (1.11–1.12) | 1.95 (1.95–1.97) | 2432 | 0 |
| hwh-4 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1883 | 7404 |
| hwh-4 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.04 (0.03–0.04) | 0.05 (0.05–0.05) | 344 | 215499 |
| hwh-4 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.03–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |
| ov | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 0 | 0 |
| ov | pnpm-install | 256518 | 242431 | 14087 | 0 | 1375 | 290 | 1.44 (1.42–1.47) | 2.22 (2.21–2.23) | 0 | 0 |
| ov | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.02) | 0.02 (0.02–0.02) | 0 | 0 |
| ov | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.05–0.05) | 0 | 0 |
| ov | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.03–0.03) | 0.03 (0.03–0.03) | 0 | 0 |
| ovv | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 0 | 0 |
| ovv | pnpm-install | 256518 | 242431 | 14087 | 0 | 1375 | 290 | 1.22 (1.20–1.37) | 2.05 (2.04–2.18) | 0 | 0 |
| ovv | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.02 (0.02–0.02) | 0 | 0 |
| ovv | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.04) | 0.05 (0.05–0.05) | 0 | 0 |
| ovv | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.03–0.03) | 0.03 (0.03–0.04) | 0 | 0 |
| xxh-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.03 (0.03–0.04) | 249 | 0 |
| xxh-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.13 (1.12–1.14) | 1.97 (1.95–1.97) | 2432 | 0 |
| xxh-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1883 | 7404 |
| xxh-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.04 (0.04–0.05) | 344 | 215499 |
| xxh-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |

### `root-tmpfs`

Rounds per configuration: hwh-32=3, ov=3

Diff time (s): overlay = walk of the upperdir; walk = walk + lstat + hash + map
comparison. Excludes `syncfs`, tar and compression.

| configuration | init | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | **total** | total + syncfs |
|---|---|---|---|---|---|---|---|---|
| hwh-32 | 0.037 (0.037–0.038) | 0.035 (0.034–0.035) | 0.834 (0.832–0.860) | 0.847 (0.835–0.857) | 0.345 (0.345–0.347) | 0.352 (0.341–0.372) | **2.46 (2.44–2.48)** | 2.46 (2.44–2.48) |
| ov | – | 0.003 (0.003–0.003) | 0.418 (0.415–0.420) | 0.028 (0.028–0.029) | 0.018 (0.018–0.022) | 0.004 (0.004–0.005) | **0.47 (0.47–0.47)** | 0.47 (0.47–0.47) |

Command run time (s) and `syncfs` after each command (s):

| configuration | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | commands total | syncfs total |
|---|---|---|---|---|---|---|---|
| hwh-32 | 0.78 (0.77–0.80) | 1.77 (1.76–1.77) | 0.23 (0.23–0.23) | 0.18 (0.18–0.18) | 0.02 (0.02–0.02) | 2.97 (2.97–2.99) | 0.00 (0.00–0.00) |
| ov | 0.77 (0.76–0.79) | 1.82 (1.81–1.87) | 0.31 (0.31–0.32) | 0.29 (0.29–0.30) | 0.03 (0.03–0.03) | 3.23 (3.22–3.29) | 0.00 (0.00–0.00) |

Diff size and export cost (medians): file = files + symlinks; tar and gz in MB;
tar s = tar to /dev/null; tgz s = tar + pgzip with 4 threads to /dev/null.

| configuration | step | walked | file | dir | whiteout | tar MB | gz MB | tar s | tgz s | hash read MB | cache hits |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hwh-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.03 (0.03–0.04) | 249 | 0 |
| hwh-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.11 (1.11–1.11) | 1.95 (1.94–1.96) | 2432 | 0 |
| hwh-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1908 | 5872 |
| hwh-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 344 | 215499 |
| hwh-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |
| ov | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.02) | 0.04 (0.03–0.04) | 0 | 0 |
| ov | pnpm-install | 256518 | 242431 | 14087 | 0 | 1375 | 290 | 1.13 (1.13–1.13) | 1.96 (1.96–1.96) | 0 | 0 |
| ov | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 0 | 0 |
| ov | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.04 (0.04–0.04) | 0 | 0 |
| ov | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.02) | 0.02 (0.02–0.03) | 0 | 0 |

### `root-cpus4-ext4`

Rounds per configuration: hwh-4=3, ovv=3

Diff time (s): overlay = walk of the upperdir; walk = walk + lstat + hash + map
comparison. Excludes `syncfs`, tar and compression.

| configuration | init | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | **total** | total + syncfs |
|---|---|---|---|---|---|---|---|---|
| hwh-4 | 0.036 (0.035–0.041) | 0.033 (0.032–0.038) | 0.981 (0.974–0.991) | 0.917 (0.875–0.934) | 0.466 (0.450–0.472) | 0.485 (0.482–0.492) | **2.92 (2.89–2.93)** | 3.64 (3.59–3.65) |
| ovv | – | 0.003 (0.003–0.003) | 0.565 (0.554–0.570) | 0.047 (0.044–0.048) | 0.030 (0.029–0.030) | 0.006 (0.005–0.006) | **0.65 (0.64–0.66)** | 1.34 (1.23–1.42) |

Command run time (s) and `syncfs` after each command (s):

| configuration | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | commands total | syncfs total |
|---|---|---|---|---|---|---|---|
| hwh-4 | 0.75 (0.74–0.77) | 2.99 (2.97–3.02) | 0.25 (0.25–0.25) | 0.22 (0.21–0.22) | 0.04 (0.04–0.04) | 4.26 (4.25–4.27) | 0.71 (0.70–0.72) |
| ovv | 0.77 (0.74–0.78) | 3.40 (3.36–3.44) | 0.45 (0.45–0.47) | 0.47 (0.46–0.47) | 0.06 (0.06–0.06) | 5.15 (5.12–5.20) | 0.69 (0.59–0.76) |

Diff size and export cost (medians): file = files + symlinks; tar and gz in MB;
tar s = tar to /dev/null; tgz s = tar + pgzip with 4 threads to /dev/null.

| configuration | step | walked | file | dir | whiteout | tar MB | gz MB | tar s | tgz s | hash read MB | cache hits |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hwh-4 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.04–0.04) | 249 | 0 |
| hwh-4 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.09 (1.09–1.10) | 2.17 (2.16–2.21) | 2432 | 0 |
| hwh-4 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1735 | 46557 |
| hwh-4 | churn-edit | 253735 | 5822 | 38 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.05–0.05) | 344 | 215499 |
| hwh-4 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.02) | 0.02 (0.02–0.03) | 344 | 215499 |
| ovv | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.04–0.04) | 0 | 0 |
| ovv | pnpm-install | 256518 | 242431 | 14087 | 0 | 1375 | 290 | 1.33 (1.33–1.36) | 2.30 (2.27–2.33) | 0 | 0 |
| ovv | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 0 | 0 |
| ovv | churn-edit | 8385 | 4009 | 4376 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.05–0.05) | 0 | 0 |
| ovv | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.02) | 0.02 (0.02–0.02) | 0 | 0 |
