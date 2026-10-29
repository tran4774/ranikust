# kaniko mode: the workdir as rootfs, one binary outside containers

| | |
|---|---|
| **Status** | Frozen, 2026-10-29 |
| **Original** | `docs-rust/02-yeu-cau-host-mode.md` (Vietnamese, not in this repository) |
| **Superseded by** | [`docs/design/kaniko-mode.md`][d-kaniko], [`docs/design/sandbox.md`][d-sandbox], [`docs/design/modes-and-detection.md`][d-modes]; [ADR-0003][adr3], [ADR-0010][adr10] |

This chapter describes **kaniko mode** (walk + hash over a single rootfs).
Overlay mode and parallel stages are covered in [06][s06]. The `RUN` sandbox
(§3) and the guard against writes outside the workdir (§4) are shared by both
modes.

## Summary

- **What does "run outside a container" mean?** `ranikust build` runs directly
  on a Linux machine (CI VM, shell runner, developer machine) with no Docker,
  containerd, `runc` or daemon, and ships as **one binary file**. Kaniko mode
  needs root; a regular user with no usable user namespace gets no mode at all,
  and the tool refuses with guidance.
- **Where does the build happen?** The working directory is the build's rootfs,
  as `/` is in kaniko. The tool never writes outside the workdir and the state
  dir, except to the output destination. The state dir moves out of the rootfs.
- **How does `RUN` run when the workdir is not `/`?** In a forked child with its
  own mount, pid, UTS and IPC namespaces, with mounts that exist only in that
  child, `pivot_root` instead of a bare `chroot`, a dropped capability bounding
  set, and stub mountpoints that are tracked and cleaned up. The sequence was
  run end to end on three images, rootless and as real root, at 1–2.5 ms
  overhead per `RUN` **[measured]**.
- **How are host files protected?** All filesystem access goes through `openat2`
  on the workdir fd. Filesystem deletion is by fd and compares `mnt_id`, not
  `st_dev`. Startup refuses `/` and system directories, requires a marker file
  in non-empty workdirs, and holds a `flock`.

## 1. Requirements

- `ranikust build ...` runs directly on a Linux machine (CI VM, shell runner,
  developer machine) without Docker, containerd, `runc` or a daemon. Only **one
  binary file** is distributed.
- It may run as root (or through `sudo`). **Kaniko mode needs root** (`chown`,
  `chroot`, switching users). A regular user on a host with no usable user
  namespace cannot run any mode; the tool must refuse and print guidance
  ([06][s06] §1).
- **The working directory is the build's rootfs**, like `/` in kaniko. The base
  image is extracted into it, `RUN` runs "inside" it, the snapshot scans it, and
  it is wiped at the end of each stage.
- The tool **never writes outside** the workdir and the state dir, except to the
  output destination (registry, tar file, OCI layout).
- Inside a container, `--workdir` defaults to `/` and the behaviour matches
  kaniko. Both cases share one code path; they differ only in the value of
  `RootDir` and in whether the `RUN` sandbox is enabled.

## 2. CLI and layout

```
ranikust build \
  --workdir /var/lib/ranikust/root \       # = RootDir; required outside a container
  --state-dir /var/lib/ranikust/state \    # = KanikoDir; defaults to <workdir>/../state on a host
  --context ./app --dockerfile ./app/Dockerfile \
  --destination registry.example.com/app:1.2.3 \
  [--tar-path out.tar | --oci-layout-path out/] [--no-push] \
  [--cache --cache-repo ...] [--build-arg K=V] [--target name]
```

```
/var/lib/ranikust/
├── root/                    --workdir = rootfs of the stage being built
│   ├── bin, etc, usr, ...   from the base image + results of the instructions
│   └── proc, dev, sys       only the image's empty directories; mounted only inside the RUN namespace (§3)
└── state/                   --state-dir, the equivalent of /kaniko
    ├── buildcontext/        extracted context (tar, git, https)
    ├── stages/<idx>         OCI layout of a stage that a later stage uses as its base
    ├── bases/<digest>       base images shared between stages
    ├── deps/<idx>           files for COPY --from
    ├── caches/<sha256(id)>  RUN --mount=type=cache, persistent across builds
    ├── secrets/             --secret
    └── layers/              temporary tars, when not streamed directly (docs/28)
```

**The state dir lives outside the workdir.** kaniko keeps `/kaniko` inside the
rootfs and adds it to the ignore list
(`kaniko@44aa7ddc0 pkg/util/fs_util.go:71-88`). That has three drawbacks: `RUN`
can still see and modify the state, `COPY --from` or `WORKDIR /kaniko` can
collide with it, and every walk has to filter one more branch. On a host there
is no reason to keep this. Inside a container, `/kaniko` can be kept for
compatibility.

## 3. Running `RUN` when the workdir is not `/`

The sequence below **was run** as a Go PoC on `alpine`, `debian` and
`node:22-bookworm-slim` (`apk add`, `apt-get install`, `npm`,
`USER nobody/node/_apt/1234:1234`), both in rootless podman at several privilege
levels ([experiment 02][e02]) and as real root on the host
([experiment 06a][e06a]) **[measured]**. As root on the host every step works
without a fallback, at about 1 ms of overhead per `RUN`.

1. **While the process still has a single thread** (before tokio or rayon
   start), call `unshare(CLONE_NEWNS)`, then `mount --make-rslave /`. From then
   on every mount made by the build **disappears when the process dies**, even
   on `SIGKILL`.
   - Measured: in a 6-thread process, `unshare(CLONE_NEWNS)` reports no error
     but moves only 1 of the 6 threads into the new namespace, and
     `unshare(CLONE_NEWUSER)` returns `EINVAL` **[measured]**.
   - In Rust: do this at the top of `main`, or split it into a helper that
     re-executes the binary itself.
2. Each `RUN` forks a child with
   `CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS | CLONE_NEWIPC`. Without
   `CAP_SYS_ADMIN`, add `CLONE_NEWUSER`.
   - The network is shared with the host by default. `--network=none` is an
     option ([ADR-0010][adr10]).
   - **`PR_SET_PDEATHSIG(SIGKILL)` is mandatory**, followed by a `getppid()`
     re-check. Without it, a killed build leaves `RUN sleep 1000` alive and
     still writing into the rootfs (observed) **[measured]**.
   - PDEATHSIG fires when the parent **thread** dies. The fork must therefore
     come from a thread that lives for the whole build, not from a tokio
     `spawn_blocking` thread.
   - With a user namespace, map **the full id range** and allow `setgroups`.
     Otherwise `USER _apt` (Debian's apt-get) fails with `setgroups: EPERM`.
   - Root inside a container can write multi-range maps itself, without
     `newuidmap`, provided the map is written once and each range fits inside
     one range of the parent map.
3. In the child, **mounts exist only in the child's namespace**, in this order
   (the order that worked):
   1. Bind the rootfs onto itself (a precondition of `pivot_root`).
   2. A new `proc`. If the container's `/proc` is masked **and** a new user
      namespace is needed, the kernel returns `EPERM`. In that case rbind the
      existing `/proc`: `RUN` then sees the container's processes, like
      BuildKit's `--oci-worker-no-process-sandbox`.
   3. A read-only `sysfs`. Mounting sysfs needs privilege over the user
      namespace that owns the network namespace, so it fails with host
      networking plus a new user namespace. Fallback: rbind `/sys`, then remount
      read-only, as rootless runc does.
   4. A tmpfs on `/dev`, with the `null`, `zero`, `full`, `random`, `urandom`
      and `tty` nodes **bind-mounted** from the outer `/dev`. `mknod` fails with
      `EPERM` at every privilege level except real root, because `CAP_MKNOD`
      must be held in the initial user namespace. Add `devpts`
      (`newinstance,ptmxmode=0666`; add `gid=5` only if gid 5 is mapped), `shm`,
      and the `ptmx`, `fd`, `stdin`, `stdout` and `stderr` symlinks.
   5. `resolv.conf`, `hosts`, `hostname`:
      - Bind **a writable copy**, not a read-only bind. Docker lets `RUN` write
        to `resolv.conf`.
      - Resolve the target **inside the rootfs** with
        `openat2(RESOLVE_IN_ROOT)`, because the target may be a symlink.
      - A remount must keep the locked flags of the source mount
        (`statfs().f_flags`), or it fails with `EPERM` inside a user namespace.
   6. `RUN --mount` mounts: cache → bind `caches/<id>`; secret → bind the file;
      bind → the context or a stage.
   7. `pivot_root(".", ".")`, then `umount2(".", MNT_DETACH)`. **Never use a
      bare `chroot`**: root inside `RUN` still holds `CAP_SYS_CHROOT` and can
      escape. As real root on the host, that escape **reaches the host's real
      `/`** (`boot`, `swap.img` and so on were visible), even with capabilities
      already dropped to Docker's set **[measured]**. Docker's default seccomp
      profile blocks `pivot_root` even with `CAP_SYS_ADMIN`; the fallback is
      `mount(rootfs, "/", MS_MOVE)` + `chroot(".")`, as runc's `--no-pivot`
      does. That fallback was tried and could not be escaped **[measured]**.
   8. `sethostname`. If seccomp blocks it, ignore the error.
   9. **Drop capabilities**: reduce the bounding and inheritable sets to
      Docker's 14 default capabilities, for **every** `USER`. Without this,
      `RUN` keeps `CAP_SYS_ADMIN` and was able to `umount /etc/resolv.conf` and
      write straight into the rootfs **[measured]**. `USER nobody` has
      `CapEff=0`, but its bounding set is still full, so a setuid binary in the
      image can regain capabilities.
   10. Resolve `USER` from the **rootfs's** `/etc/passwd` and `/etc/group`, then
       `setgroups`/`setgid`/`setuid`. A numeric uid with no passwd entry gets
       gid 0, as in Docker.
   11. `chdir(WORKDIR)`. Look up the executable in `PATH` **after** `pivot_root`
       and **after** `setuid`. `execve`.
4. The parent waits for the child to exit. The first process in a pid namespace
   is its init; when it exits, the kernel kills everything else (checked with
   `setsid sleep 1000 &`) **[measured]**. Neither `tini` nor `kill(-pgid)` is
   needed (`kaniko@44aa7ddc0 pkg/commands/run.go:346-348`, `:397-405`). Caveat:
   a command running as pid 1 ignores SIGTERM unless it installs a handler.
   Docker and BuildKit behave the same way.
5. **Stub mountpoints.** An image may lack `/etc/resolv.conf` (`alpine:3.20`),
   `/etc/hosts` (`debian`) or `/proc`, so an empty file or directory must be
   created **on the real rootfs** as a bind target. A stub is a real file: it
   leaks into the layer if it is not cleaned up, and it is left behind if the
   build is killed (observed) **[measured]**. The procedure, modelled on
   BuildKit's `executor/stubs.go`:
   - write the stub list to the state dir **before** creating the stubs;
   - delete the stubs after `RUN`, before taking the diff;
   - on startup after a crash, clean up from the list.

Measured overhead: **1–2.5 ms per `RUN`**, including Go start-up on re-exec.
Negligible **[measured]**.

Because the mounts live in the child's namespace, when the parent takes the diff
it sees `proc`, `dev` and `sys` only as the image's empty directories, and sees
the image's original `resolv.conf`/`hosts`. **No ignore-list entries are
needed** for these paths. Stubs are the exception and are handled as in step 5.

Notes:

- Inside a container (`--workdir=/`), step 3 is skipped and `RUN` runs as in
  kaniko. If the container has `CAP_SYS_ADMIN`, a pid namespace can still be
  used to clean up leftover processes.
- On a host, `RUN --mount=type=cache` uses a bind mount instead of `swapDir`
  (`kaniko@44aa7ddc0 pkg/commands/run.go:83-135`, `:563-597`). No directory is
  renamed and renamed back, so a `RUN` that fails half-way leaves no
  inconsistent state.
- Writing this part takes about 1,000–1,500 lines with `rustix`/`nix` (the Go
  PoC is about 840 lines, without `RUN --mount`) **[inferred]**. youki's
  `libcontainer` **[external]** is not needed: it targets the full OCI runtime
  spec (cgroups, seccomp, hooks, on-disk container state).

## 4. Parameterizing `RootDir` and blocking writes outside the workdir

Where kaniko uses the process's `/`, and what the Rust version does instead
(details in [01][s01] §3):

| Concern | kaniko | ranikust |
|---|---|---|
| Paths inside the image | String joins with `config.RootDir` in 37 places | A single `RootPath` type; every filesystem operation goes through `openat2` on the workdir fd |
| Escaping the root during extraction/`COPY` | `filepath-securejoin` (`kaniko@44aa7ddc0 pkg/util/fs_util.go:238-244`) | `openat2(RESOLVE_IN_ROOT \| RESOLVE_NO_MAGICLINKS)`: the kernel guarantees that symlinks and `..` cannot point outside the workdir |
| Users/groups | `os/user` reads the process's `/etc/passwd` | Parse `<workdir>/etc/passwd` and `<workdir>/etc/group` directly |
| Executable lookup | `os.Setenv("PATH")` + `exec.LookPath` on `/` | Look up inside the workdir, or let `execvp` run after `chroot` |
| Ignore list | `/proc/self/mountinfo` + defaults (`/kaniko`, `/var/run`) | Mount points **under the workdir** from mountinfo + `--ignore-path`. Usually empty on a host |
| `DeleteFilesystem` | Walk `/`, delete everything except the ignore list (`kaniko@44aa7ddc0 pkg/util/fs_util.go:292-324`) | Delete the workdir's contents with `openat`/`unlinkat` by fd, **never following symlinks** and never crossing mount points. **Comparing `st_dev` is not enough**: a same-filesystem bind mount inside the workdir gets wiped ([experiment 09][e09], **[measured]**). Compare `mnt_id` (`statx` `STATX_MNT_ID` or `STATX_ATTR_MOUNT_ROOT`) against mountinfo |

Safety checks at startup (on a host):

- refuse a `--workdir` that is `/` or an ancestor of `/usr`, `/etc`, `/home`,
  `/var/lib` and the like. Compare by `(dev, ino)` after opening, not by string:
  `//` slips past a string comparison;
- refuse a non-empty workdir that lacks ranikust's marker file
  (`.ranikust-root`), so that a mistyped directory is never wiped;
- take a `flock` on the workdir: only one build may run in a workdir at a time.

To run several builds in parallel on one host, use several workdirs.

## 5. Overlay and parallel stages

Moved to [06][s06]. In short:

- with mount privileges (on a host, or in a privileged container), use
  overlayfs: each stage gets an overlay rootfs inside the workdir, and stages
  run in parallel;
- without them, use kaniko mode as described in §2–§4 of this chapter;
- fanotify is not used.

## References

1. [Experiment 02: RUN sandbox][e02].
2. [Experiment 06a: Root re-run][e06a].
3. [Experiment 09: kaniko-mode prototype][e09].
4. kaniko at `44aa7ddc0`: `pkg/util/fs_util.go:71-88`, `:238-244`, `:292-324`;
   `pkg/commands/run.go:83-135`, `:346-348`, `:397-405`, `:563-597`.
5. BuildKit `executor/stubs.go` and the `--oci-worker-no-process-sandbox`
   option, <https://github.com/moby/buildkit> **[external]**, accessed
   2026-10-29.
6. runc `--no-pivot` and rootless sysfs handling,
   <https://github.com/opencontainers/runc> **[external]**, accessed 2026-10-29.
7. youki `libcontainer`, <https://crates.io/crates/libcontainer> **[external]**,
   accessed 2026-10-29.
8. kaniko optimization notes, docs/28 stream-tar-compress (external, not in this
   repository).
9. Survey chapters: [01 kaniko today][s01], [06 Overlay and parallelism][s06].

[s01]: 01-kaniko-today.md
[s06]: 06-overlay-and-parallelism.md
[e02]: ../experiments/02-run-sandbox.md
[e06a]: ../experiments/06a-root-rerun.md
[e09]: ../experiments/09-kaniko-mode-prototype.md
[adr3]: ../../adr/0003-dockerfile-trust-and-default-isolation.md
[adr10]: ../../adr/0010-run-network-host-or-none.md
[d-kaniko]: ../../design/kaniko-mode.md
[d-sandbox]: ../../design/sandbox.md
[d-modes]: ../../design/modes-and-detection.md
