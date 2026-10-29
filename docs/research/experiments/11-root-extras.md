# Experiment 11: Root extras

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Does a path-bound AppArmor profile let an unprivileged user run rootless overlay mode on Ubuntu with `restrict_unprivileged_userns=1`, and do the core overlay results hold on ext4, xfs and btrfs, including `volatile` and nested overlays? |
| **Answer** | Yes to both. A 3-line profile (`userns,` + `flags=(unconfined)`) bound to the binary's absolute path enabled the full overlay sequence, a fresh `proc` included, while the same binary at another path stayed blocked. ext4, xfs and btrfs behaved identically; a `volatile` upper/work pair cannot be mounted again until `work/incompat/volatile` is removed, and overlays nest only to depth 2. |
| **Related** | [ADR-0016](../../adr/0016-overlay-mount-options.md), [ADR-0004](../../adr/0004-support-every-privilege-level.md), [modes and detection design](../../design/modes-and-detection.md), [overlay snapshot design](../../design/overlay-snapshot.md), experiments [01](01-overlay-semantics.md), [02](02-run-sandbox.md), [06a](06a-root-rerun.md), [13](13-docker-rootful-vm.md) |
| **Original** | `docs-rust/poc/11-root-extra/` (Vietnamese, not in this repository) |

## Summary

- Two items that the [survey summary](../survey/00-summary.md) §4 and
  [survey 06](../survey/06-overlay-and-parallelism.md) §1 still listed as
  unverified needed root: an AppArmor profile for the binary (`aa`), and
  overlay on xfs and btrfs plus nested overlays (`fs`). A script was prepared
  and dry-run without root, then run once as real root in the initial user
  namespace via `pkexec`.
- **AppArmor:** before the profile, the probe's `unshare` succeeded but the
  process was moved into the `unprivileged_userns` profile and every
  following step failed with `EACCES`. With a 3-line profile bound to the
  binary's absolute path, all 8 steps of the overlay sequence succeeded.
  The same binary at another path, util-linux `unshare -Urm`, and the binary
  after the profile was removed were all blocked again **[measured]**.
- The profile is bound to a **path**, not to the binary's content, so the
  binary must live in a root-owned path (`/usr/bin`, `/usr/local/bin`), never
  in `~/bin` **[measured]**; installing it elsewhere would hand out
  unrestricted user namespaces **[inferred]**.
- **Filesystems:** ext4, xfs and btrfs gave **identical** results, matching
  ext4 and tmpfs in earlier experiments: the recommended option set mounts as
  root and in a userns, whiteouts share an inode, copy-up breaks hardlinks,
  lower-directory renames return `EXDEV`, and `lowerdir+` stops at 500 layers
  **[measured]**.
- **`volatile` lifecycle:** after unmounting, the workdir keeps
  `incompat/volatile/dirty`, and mounting the same upper/work again fails with
  `EINVAL` with or without `volatile`. A fresh upper/work over the old upper
  works, as does removing `work/incompat/volatile` first **[measured]**.
- **Nesting:** depth 2 (lower is an overlay) works; depth 3 fails, and an upper
  on an overlay fails, as root and in a userns, on every filesystem
  **[measured]**.

## Questions

1. **`aa`:** on Ubuntu ≥ 24.04 (`kernel.apparmor_restrict_unprivileged_userns=1`),
   does an AppArmor profile bound to the binary's path, modelled on
   `/etc/apparmor.d/podman` (`flags=(unconfined) { userns, }`), really let a
   normal user run rootless overlay mode? This is the guidance that
   [survey 06](../survey/06-overlay-and-parallelism.md) §1 step 4 planned to
   print for users.
2. **`fs`:** do the core overlay tests give the same results with upper and
   lower on ext4, xfs and btrfs (loop devices)?
3. How deep can overlays be nested, and can an upper live on an overlay?
4. What happens when an upper/work pair used with `volatile` is mounted
   again?

## Environment

- **Date:** 2026-10-29, root run at 08:41.
- **Host:** Ubuntu 26.04, kernel 7.0.0-34-generic,
  `restrict_unprivileged_userns=1`, AppArmor enabled (`aa-status --enabled`:
  yes), AppArmor parser 5.0.2. The kernel had `xfs` and `btrfs` loaded
  (`/proc/filesystems`).
- **Privilege:** real root in the initial user namespace via `pkexec`: uid 0,
  `CapEff`/`CapBnd` `000001ffffffffff`, AppArmor `unconfined`. The
  unprivileged test user was uid/gid 1000, run with
  `setpriv --reuid --regid --clear-groups`.
- **Isolation of the test itself:** the script re-executed itself in
  `unshare -m --propagation private`; `/tmp` was a 3 GB tmpfs visible only in
  that namespace.
- **Filesystem images:** 512 MB sparse files (about 87 MB actually used),
  created with `mkfs` as a normal user in a `debian:latest` container (the
  host has no `mkfs.xfs`/`mkfs.btrfs`): xfsprogs 6.13.0, btrfs-progs 6.14,
  e2fsprogs 1.47.2. Mount options as reported: xfs
  `rw,inode64,logbufs=8,logbsize=32k,noquota`; btrfs
  `rw,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/`; ext4 `rw`.
- **Probe:** `userns-probe`, Rust, about 120 lines, a static musl binary of
  464 KB built as a normal user in `rust:1-alpine`.

## Method

### Probe

`userns-probe` runs exactly the sequence of rootless overlay mode, reporting
each step:

| Step | Action |
|---|---|
| 1 | `unshare(NEWUSER\|NEWNS)` |
| 2 | write `setgroups`/`uid_map`/`gid_map` |
| 3 | `mount --make-rprivate /` |
| 4 | mount a tmpfs |
| 5 | mount overlay with `userxattr`, the recommended option set and `volatile` |
| 5b | delete a lower file and check for a 0/0 whiteout |
| 6 | `unshare(NEWPID)` + fork |
| 6b | mount a new `proc` in the PID namespace |

### `aa`: AppArmor profile for the binary

1. Copy the probe to **two root-owned copies** in `/tmp/rk11/aa/` (the
   namespace's private tmpfs): `bin/userns-probe` (in the profile) and
   `other/userns-probe` (same content, different path).
2. Write the profile to `/tmp/rk11/aa/ranikust-test-probe`, **not** to
   `/etc/apparmor.d`. As run:

   ```
   abi <abi/5.0>,
   include <tunables/global>

   profile ranikust-test-probe /tmp/rk11/aa/bin/userns-probe flags=(unconfined) {
     userns,
     @{exec_path} mr,
   }
   ```

   The prepared script also listed the user-writable copy in `stage/bin/`; it
   was removed from the profile before the run so that no hole was open while
   the profile was loaded (see Limitations).
3. As the normal user:
   1. **before** loading: run the probe, and `unshare -Urm` for comparison;
   2. load with `apparmor_parser --skip-cache -r` (no cache written to
      `/var/cache/apparmor`);
   3. **after** loading, run the root-owned copy (3), the copy in `stage/`
      (3b), the copy at the other path (3c, must stay blocked) and
      `unshare -Urm` (3d, must stay blocked);
   4. unload with `apparmor_parser --skip-cache -R` and check
      `/sys/kernel/security/apparmor/profiles`;
   5. run the probe again **after** unloading (must be blocked again).
4. An EXIT trap (also on error or Ctrl-C) unloads the profile if it is still
   loaded, falls back to writing its name to
   `/sys/kernel/security/apparmor/.remove`, and prints the state.

### `fs`: ext4, xfs, btrfs and nested overlays

For each image:

1. `cp --sparse` to `/tmp/rk11` (tmpfs); the original in `stage/` is not
   changed.
2. `losetup -f --show`, `mount`, then **`losetup -d` immediately**: the loop
   device has autoclear set and detaches on umount, even if the namespace
   dies.
3. Run the tests, with every lower, upper and work directory on that
   filesystem:
   - **A.** The option matrix, including the recommended set with `volatile`,
     as real root and as root + `unshare -Urm`.
   - **B1–B3.** Experiment 01's upper-semantics script: `userxattr` + the
     recommended set (root); `trusted` + `redirect_dir=off` (root);
     `userxattr` in `unshare -Urm`. **Without `volatile`**, because the script
     mounts the same upper/work again, which always fails with `EINVAL` when
     `volatile` is set (see V).
   - **V.** The `volatile` lifecycle: mount, write, umount; mount the same
     upper/work again (with and without `volatile`); use the old upper as a
     lower; remove `work/work/incompat/volatile` and mount again.
   - **C.** Shortened lowerdir limits: legacy `mount(2)` with N ≤ 100, and
     `fsconfig(lowerdir+)` with N = 2, 100, 500, 501.
   - **D.** Nested overlays: depth 1 (lower = real filesystem); depth 2 (lower
     = a depth-1 overlay); depth 3 (lower = a depth-2 overlay, expected to
     fail); upper on an overlay (expected to fail). As root, then again in
     `unshare -Urm`, followed by the overlay lines of `dmesg`.
4. Unmount and delete the copy.

A `fsdir=<directory>` argument runs A–D on an already-mounted directory
without loop devices; it was used for the dry run.

### Changes to the system and how they are undone

| Change | Scope | Undone automatically | Manual undo if the script is `SIGKILL`ed |
|---|---|---|---|
| AppArmor profile `ranikust-test-probe` loaded into the kernel | System-wide, a few seconds | Yes (step 4 and the trap) | `echo -n ranikust-test-probe \| sudo tee /sys/kernel/security/apparmor/.remove`, check with `sudo grep ranikust /sys/kernel/security/apparmor/profiles`. **A reboot also removes it**: there is no file in `/etc/apparmor.d` and no cache entry |
| Loop device (`/dev/loopN`) | System-wide | Yes (autoclear + `losetup -d` in the trap) | `losetup -a \| grep rk11`, then `sudo losetup -d /dev/loopN` |
| Mounts (tmpfs `/tmp`, xfs, btrfs, ext4, overlay) | Only in the private mount namespace | Gone when the script ends | Not needed |
| Files: image copies, probe, profile | The namespace's private tmpfs | Gone automatically | Not needed |
| `out/*.txt` | PoC directory | Not deleted; handed back to the user | `rm -rf out/` |

The script does **not** write to `/etc`, `/var/cache/apparmor` or `/usr`,
installs no packages and changes no sysctl.

## Results

All results are **[measured]** unless labelled otherwise.

### Before the root run

**The probe as a normal user on the host, without a profile:**

```
INFO uid=1000 ... apparmor=unconfined restrict_unprivileged_userns=1
RESULT 1 unshare(NEWUSER|NEWNS)           OK
INFO after unshare: apparmor=unprivileged_userns (enforce) CapEff: 000001ffffffffff
RESULT 2 write setgroups/uid_map/gid_map  FAIL Permission denied (os error 13)
RESULT 3 mount --make-rprivate /          FAIL Permission denied (os error 13)
RESULT 4 mount tmpfs                      FAIL Permission denied (os error 13)
```

(Output labels translated from the Vietnamese original.) `unshare`
succeeds, but the process is immediately moved into the
`unprivileged_userns` profile. Writing `uid_map` fails with `EACCES` and every
`mount` fails with `EACCES`, even though `CapEff` still shows every
capability. This matches [experiment 02](02-run-sandbox.md) §2 and
[experiment 01](01-overlay-semantics.md) §1.d, and is the "before" state of
the `aa` test.

**Dry run of the whole script in `podman run --privileged`** (root of a
rootless userns; no AppArmor and no loop devices in the container):

- The `aa` flow ran to the end. `apparmor_parser` is not in the container, so
  the "load failed" branch was exercised. Without AppArmor in the way, the
  probe reported OK for all 8 steps. The trap ran.
- The `fs` flow: `losetup` was refused, so all three images were `SKIP`ped;
  the trap reported 0 loop devices left attached.
- `fsdir=/t` (tmpfs):
  - **A:** the recommended set
    `userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off,uuid=off,volatile`
    mounted; `redirect_dir=off` returned `EINVAL`, as in experiment 01.
  - **V:** `volatile` leaves `work/work/incompat/volatile/dirty` behind.
    Mounting the same upper/work again then fails with **`EINVAL`, even
    without `volatile`**. Using the old upper as a lower (new upper/work)
    works; removing `work/work/incompat/volatile` and mounting again works.
    The earlier root run ([experiment 06a](06a-root-rerun.md)) had already hit
    this without recording it: in its semantics output, configurations F and G
    (with `volatile`) ended with `OSError: [Errno 22]` on the re-mount.
  - **C:** `lowerdir+` worked up to 500 layers; layer 501 returned `EINVAL`
    (`overlay: too many lower directories, limit is 500`).
  - **D:** depth 2 (lower is an overlay) worked; **depth 3 failed**; an upper
    on an overlay failed; both as container root and in `unshare -Urm`. The
    `FILESYSTEM_MAX_STACK_DEPTH = 2` limit cited in experiment 01 §5 is
    therefore now **measured**, not only read.

### `aa`: AppArmor profile for a normal user, Ubuntu 26.04 (`restrict_unprivileged_userns=1`)

In the recorded run the profile was bound only to the root-owned copy
(`/tmp/rk11/aa/bin/userns-probe`), so case 3b (the copy in `stage/`) was
blocked. At the end the profile had been removed from the kernel (0 left) and
no loop device remained.

| Case | userns | write `uid_map` | mount | overlay `userxattr` + `volatile` | new `proc` |
|---|---|---|---|---|---|
| 1. Before loading the profile | OK, but moved into profile `unprivileged_userns` | **EACCES** | EACCES | – | – |
| 3. After loading, the path named in the profile | OK, profile `ranikust-test-probe (unconfined)` | OK | OK | **OK**, 0/0 whiteout correct | **OK** |
| 3b, 3c. After loading, the same binary at another path | As case 1 | EACCES | EACCES | – | – |
| 3d. util-linux `unshare -Urm` | `uid_map` EPERM | | | | |
| 5. After removing the profile | As case 1 | | | | |

A 3-line profile (`userns,` + `flags=(unconfined)`) bound to the **absolute
path** of the binary is enough for a normal user to run the full rootless
overlay sequence, a new `proc` included. The profile is bound to the path, not
to the binary's content.

### `fs`: ext4, xfs, btrfs (loop devices), real root and root + `unshare -Urm`

The three filesystems gave **identical** results, which also match ext4 and
tmpfs in the earlier experiments:

- The recommended set
  `userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off,uuid=off,volatile`
  mounts, both as root and in a userns. `redirect_dir=off` is accepted only as
  root and without `userxattr`.
- Upper semantics: whiteouts share one inode, copy-up breaks hardlinks,
  `rename(2)` of a lower directory returns `EXDEV`, and xattrs are `trusted.*`
  or `user.*` depending on the mode.
- The 500-layer limit with `lowerdir+`.
- **`volatile` lifecycle:** after umount the workdir still holds
  `work/incompat/volatile/dirty`. Mounting the same upper/work again fails
  with `EINVAL`, with or without `volatile`. Using the old upper as a lower,
  with a new upper/work, works. Removing `work/work/incompat/volatile` and
  mounting again also works.
- **Nested overlays:** depth 2 works, depth 3 fails; an upper on an overlay
  fails. The results are the same as root and in a userns. The kernel logs
  `overlayfs: maximum fs stacking depth exceeded` and
  `overlay: filesystem on …/up not supported as upperdir`.

## Findings

1. A profile of the form
   `profile <name> /abs/path flags=(unconfined) { userns, @{exec_path} mr, }`
   lets a normal user on Ubuntu with `restrict_unprivileged_userns=1` run the
   whole rootless overlay sequence, a new `proc` included **[measured]**
   (`aa` table, case 3).
2. The profile applies only to that exact path: the same binary elsewhere and
   `unshare -Urm` stay blocked, and removing the profile restores the block
   **[measured]** (cases 3b–5).
3. While such a profile is loaded, anyone who can write to the profiled path
   can place another binary there and obtain a user namespace with every
   capability; the guidance is safe only for a root-owned path such as
   `/usr/local/bin/ranikust` (mode 0755, owner root), as podman and buildah
   install through packages **[inferred]**.
4. `flags=(unconfined)` gives the binary the same freedom as podman; this is
   the price of the template survey 06 §1 planned to recommend
   **[inferred]**.
5. ext4, xfs and btrfs behave identically for every overlay property tested
   **[measured]** (`fs` results).
6. A `volatile` upper/work pair cannot be mounted again until
   `work/incompat/volatile` is removed; a fresh upper/work over the old upper
   is unaffected **[measured]** (V).
7. Overlays nest to depth 2 only, and an upper cannot be on an overlay, as
   root and in a userns **[measured]** (D).

## Implications for ranikust

- When AppArmor blocks unprivileged user namespaces, ranikust can print
  guidance for a path-bound profile, valid only for a binary installed in a
  root-owned path; otherwise it falls back or refuses with that guidance
  ([ADR-0004](../../adr/0004-support-every-privilege-level.md),
  [modes and detection design](../../design/modes-and-detection.md)).
- The default overlay scheme, a fresh upper per command with the previous
  upper becoming a lower, is not affected by the `volatile` lifecycle. Any
  design that keeps one mount for several commands and re-mounts it must
  remove `incompat/volatile` first (and `syncfs` if durability is needed)
  ([survey 06](../survey/06-overlay-and-parallelism.md) §3.1,
  [ADR-0016](../../adr/0016-overlay-mount-options.md),
  [overlay snapshot design](../../design/overlay-snapshot.md)).
- The same mount options are valid on ext4, xfs and btrfs
  ([ADR-0016](../../adr/0016-overlay-mount-options.md)).
- Upper and work directories cannot be placed on an overlay, and a lower that
  is already an overlay cannot be nested further; mode detection has to take
  the filesystem under the workdir into account
  ([modes and detection design](../../design/modes-and-detection.md)).

## Limitations and open questions

- **The user-writable copy.** The prepared script deliberately listed
  `stage/bin/userns-probe`, in a user-writable directory, in the profile to
  demonstrate Finding 3 (case 3b). For the recorded run that path was removed
  from the profile, so the hole was not opened and case 3b only shows the
  block; the exposure itself is **[inferred]**, not demonstrated.
- **B3 did not run.** On all three filesystems, the semantics script in
  `unshare -Urm` failed with `python3: can't open file …: Permission denied`:
  the script lives under `/home`, which the mapped uid cannot traverse (the
  same environment issue as in [experiment 06a](06a-root-rerun.md)). Userns
  coverage on these filesystems comes from A and D only **[measured]**.
- Python ran `ngu-nghia.py` and `lowerdir.py` from experiment 01 **as root**;
  they should be read before re-running.
- Mounting filesystem images through the kernel driver always carries some
  driver-security risk for untrusted images. These images were created clean
  by the PoC, not downloaded.
- If neither `PKEXEC_UID` nor `SUDO_UID` is set, the unprivileged tests run as
  `nobody` (65534), which may not be able to read `stage/` under `/home`, and
  case 3b becomes meaningless.
- btrfs has its own inode numbers and `st_dev` per subvolume; `xino` and inode
  numbers in the B dumps were not examined. It may differ from ext4 in inode
  numbers but not in semantics **[inferred]**.

## Reproduction

The PoC sources are archived outside this repository, under
`docs-rust/poc/11-root-extra/`:

| File | Role |
|---|---|
| `run-as-root.sh` | The root script. Modelled on experiment 06a's script: re-exec in `unshare -m --propagation private`, `/tmp` shadowed by a 3 GB tmpfs, cleanup trap, results handed back to `${SUDO_UID:-$PKEXEC_UID}` |
| `probe/` | Source of `userns-probe` (Rust, about 120 lines) |
| `stage/bin/userns-probe` | Prebuilt static musl binary, 464 KB, built as a normal user in `rust:1-alpine` |
| `stage/img/{ext4,xfs,btrfs}.img` | 512 MB sparse images, `mkfs` done in `debian:latest`; versions in `stage/img/versions.txt` |
| `lowerdir-ngan.py` | Shortened lowerdir limits (N = 2, 100, 500, 501), reusing the functions of experiment 01's `lowerdir.py` |
| `out/` | `aa-profile.txt`, `fs-ext4.txt`, `fs-xfs.txt`, `fs-btrfs.txt`, `run.log` |

The script also uses experiment 01's `ngu-nghia.py` (upper semantics) and
needs `apparmor_parser`, `aa-status`, `setpriv`, `losetup`, `unshare` and
`python3` on the host. No network is needed, and the `nk-*` containers are not
touched. Expected run time: under a few minutes **[inferred]**.

```sh
pkexec bash docs-rust/poc/11-root-extra/run-as-root.sh
# or only one part:  ... run-as-root.sh aa     |   ... run-as-root.sh fs
```

## References

1. [Experiment 01: Overlay semantics](01-overlay-semantics.md) §1.d, §5.
2. [Experiment 02: RUN sandbox](02-run-sandbox.md) §2.
3. [Experiment 06a: Root re-run](06a-root-rerun.md).
4. [Survey 00: Summary](../survey/00-summary.md) §4.
5. [Survey 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md)
   §1, §3.1.
6. `/etc/apparmor.d/podman` on the test host (template for the profile).
