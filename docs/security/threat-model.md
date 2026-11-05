# Threat model

| | |
|---|---|
| **Status** | Phase 2 code. The mitigations of T1 to T12 and T17 to T22 are built, except what is planned below; T14 is by design; T13, T15 and T16 are built in part (T13: digests, atomic renames and per-digest locks, but the versioned cache keys wait for the layer cache, Phase 4; T15: layers are streamed, tar extension headers are not bounded, Q-056; T16: property tests cover the parser, the ignore matcher and tar round trips, and `cargo fuzz` waits for Phase 6). The Evidence column of each row says how far it is shown. **Tested**, where the row names a test: T1 to T11 (credential handling for T11), T17 to T22 (T3: path-level races only, `RUN` and stage races are not yet tested). **No test named in the row**, resting on a pre-code experiment **[measured]** or on a review or inference **[inferred]**: T12 to T16. **Planned, not built**: `--userns` and rootless mode (Phase 3), and a hardened `chroot` for `--parallel-stages` (T12; the flag is refused). T23 (the release channel) is built in the release workflow; the repository settings it relies on (the rulesets, the `release` environment) are the maintainer's, and its tag path first runs with the first release. §7 lists what is still unverified. |
| **Scope** | The `ranikust` binary, its state directory and store, the build roots it makes there (or `/` in a container), and the processes it starts for `RUN`; and the release channel: the binaries the release workflow publishes and the identity that attests them (T23). |
| **Used by** | [`SECURITY.md`](../../SECURITY.md) (what is a vulnerability), reviewers and the `reviewer`, `host-safety` and `security-audit` agents (how severe a finding is). |
| **Keep in sync** | Update this file in the same pull request as any change to isolation, privileges, paths written, credential handling or store sharing. |

Evidence labels follow [`docs/research/TEMPLATE.md`](../research/TEMPLATE.md):
**[measured]**, **[code]**, **[external]**, **[inferred]**.

## 1. Why ranikust is security-sensitive

- It runs Dockerfile `RUN` commands as uid 0, outside containers as real root
  on the host, separated only by namespaces
  ([survey 02](../research/survey/02-kaniko-mode-requirements.md) §3).
- It extracts image layers, copies build contexts and parses Dockerfiles, all
  of which come from outside.
- In kaniko mode it empties a whole root filesystem between stages, and
  inside a container that root is `/`.
- It holds registry credentials and build secrets.
- Builds may run concurrently and share one store
  ([ADR-0007](../adr/0007-run-anywhere-shared-store.md)).

## 2. Assets

| ID | Asset | Harm if lost |
|---|---|---|
| A1 | Host files outside the state directory, store, requested outputs and, in kaniko mode in a container, `/` | Data loss or host compromise |
| A2 | The host's privileges: root, capabilities, the host's namespaces | Host compromise |
| A3 | Registry credentials (docker `config.json`, credential helpers, tokens) | Registry takeover, supply-chain attack |
| A4 | Build secrets (`RUN --mount=type=secret`, `--secret`) | Secret disclosure |
| A5 | The store and caches: blobs, extracted layers, cache mounts, registry cache | Poisoned images in later builds |
| A6 | The integrity of the output image | Shipping content the Dockerfile did not ask for |
| A7 | Availability of the host: disk, memory, processes | Denial of service |
| A8 | The release channel: the binaries published as ranikust releases, and the identity that attests their provenance (the release workflow run for a `v*` tag) | Every user who installs a release runs code nobody reviewed: a supply-chain attack |

## 3. Inputs and how much they are trusted

| Input | Trust |
|---|---|
| The operator: flags, store location, output paths | Trusted. ranikust protects the operator from typing mistakes (a wrong `--state-dir`: the store's marker refuses a foreign non-empty directory), not from intent. A path the operator types is still not allowed to run through names the build context's owner controls (next row). |
| The build context's owner: whoever owns the context directory or commits to the repository checked out there | A principal of their own, trusted less than the operator ([ADR-0076](../adr/0076-the-build-context-owner-is-a-lower-trust-principal.md)): a merge request's author, a third-party repository, an unprivileged account whose directory root builds. They control every name in the context, symlinks and hardlinks included, and none may steer ranikust's own host reads or writes (T22). |
| Dockerfile and build context | The operator decides whether to build them ([ADR-0003](../adr/0003-dockerfile-trust-and-default-isolation.md)). `RUN` commands get the mode's isolation (§4). ranikust's own code that reads them (parser, `COPY`, `.dockerignore`) treats them as hostile. |
| Base images and layers from a registry | Hostile to ranikust's own code: layer entries may name `..`, absolute paths, symlinks, hardlinks, device nodes, setuid files and whiteouts aimed outside the root. |
| Registry responses | Hostile: verified by digest. |
| Other builds sharing the store | May belong to another trust domain; only content-addressed data is shared. |
| The kernel and the outer container runtime | Trusted; their bugs are out of scope. |

## 4. Trust boundaries by mode

The mode is chosen at startup by actually trying each step
([ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0004](../adr/0004-support-every-privilege-level.md),
[survey 06](../research/survey/06-overlay-and-parallelism.md) §1).

| Mode | Typical environment | What separates `RUN` from the host | Strength |
|---|---|---|---|
| Overlay, root | Host as root or via sudo; `--privileged` container; Podman with `CAP_SYS_ADMIN` | New mount, PID, UTS and IPC namespaces; `pivot_root` (or `MS_MOVE` + `chroot`); capabilities cut to Docker's 14 defaults for every `USER`; `PR_SET_PDEATHSIG`; Docker's default seccomp profile (`--seccomp` changes it, [ADR-0109](../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)); a user namespace (`--userns`) is planned, Phase 3, not built | About `docker build` with a root daemon: a kernel bug or a retained capability crosses it |
| Overlay, rootless (planned, Phase 3; not built) | Podman rootless; regular user on Debian 12+, Fedora, RHEL 9+, Ubuntu 22.04 | As above, inside a user namespace: root in `RUN` maps to the invoking user | About rootless Podman: the invoking user's privileges at most |
| kaniko, mounts available | Root where overlay cannot be used (NFS, FUSE, no overlay module) | Same sandbox as overlay root, one directory per stage | As overlay root |
| kaniko, no mounts | Docker's default profile, ordinary Kubernetes pods | No boundary to the container: `RUN` runs in ranikust's own container on rootfs `/`, as in kaniko. ranikust's own state and credentials are kept out of reach (T21, [ADR-0051](../adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)) | The outer container only, for everything but ranikust's state |
| kaniko, no mounts, `--parallel-stages` | Refused ([ADR-0060](../adr/0060-copy-leaves-special-files-out-where-run-has-no-nodev.md)); planned in [ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md) | A bare `chroot` per stage, no `/proc` (not built) | None: a `RUN` escapes the `chroot` and reads or writes other stages' roots and secrets **[measured]** |
| None | Regular user on Ubuntu 24.04+ without an AppArmor profile; Kubernetes PSS restricted | ranikust refuses to start a build | — |

A bare `chroot` is not a boundary: root with `CAP_SYS_CHROOT` leaves it, and
run as root on a host it reaches the host's real `/` **[measured]**
([experiment 02](../research/experiments/02-run-sandbox.md),
[experiment 06a](../research/experiments/06a-root-rerun.md),
[experiment 13](../research/experiments/13-docker-rootful-vm.md) §1). It is
therefore planned only where nothing better is possible: kaniko mode with
`--parallel-stages` inside a container that forbids mounts, which is
refused (ADR-0060).

## 5. Threats and mitigations

| ID | Threat | Assets | Modes | Required mitigation | Evidence |
|---|---|---|---|---|---|
| T1 | A layer entry, `COPY` source or `WORKDIR` path escapes the root through `..`, an absolute symlink, a symlink chain or a whiteout aimed outside | A1, A6 | all | Resolve every path in a root with `openat2(RESOLVE_IN_ROOT \| RESOLVE_NO_MAGICLINKS)` through `rnk_rootfs::RootPath`; never join strings onto a host path. The `RUN` child enters `WORKDIR` the same way (`fchdir`), then checks with `getcwd` that it is below the root | [survey 02](../research/survey/02-kaniko-mode-requirements.md) §4; `RootPath` escape suite, T0 (`crates/rnk-rootfs/tests/escape.rs`); `copy` with symlinks out of both roots, T0 (`crates/rnk-rootfs/src/copy/tests.rs`); T2 `red_team_an_inherited_descriptor_gives_a_run_no_way_out` [measured]; planted stub records aimed outside and stubs swapped for symlinks out of the root, T0 `red_team_planted_stub_records_stay_in_the_root`, `red_team_stubs_swapped_for_links_out_or_nodes_are_left_with_the_host` (`crates/rnk-sandbox/src/journal.rs`) |
| T2 | A hardlink entry links to a host file, so later writes change it | A1 | all | Resolve hardlink targets inside the root; refuse targets outside it | [survey 04](../research/survey/04-risks-and-roadmap.md) §4; `extract` refuses targets climbing out and links inside the root only; `extract_overlay` copies a target from the lower layers, walked by name without following symlinks, T0 (`crates/rnk-rootfs/src/extract/tests.rs`) [measured] |
| T3 | A race between check and use: a `RUN` process or another stage swaps a directory for a symlink | A1 | all, mostly parallel stages | Operate relative to open directory descriptors with `O_NOFOLLOW` and `RESOLVE_*` flags; never re-resolve a checked path | `RootPath`, `extract`, `delete_contents` and `copy`: a thread swapping a directory for a symlink out, T0 (`crates/rnk-rootfs/tests/escape.rs`, `src/*/tests.rs`) [measured]; `RUN` and stage races not yet tested |
| T4 | Stage cleanup deletes user data | A1 | kaniko | No operator-named build root: ranikust builds on `/` only in a container (ADR-0048), else in a directory per stage, `<state>/builds/<id>/stages/<n>/rootfs`, each made by ranikust and checked to be the directory made (ADR-0050, ADR-0059), a root handed to the only stage that reads it moved only between two stage directories' descriptors, checked before and after the move, and pruned through descriptors first, the machine's own `/` and a mount below it refused, never when a file in it is hardlinked from outside it (ADR-0061, ADR-0062); the state directory is owned by the user, `0700`, below directories only root may write, checked by descriptor; the state directory accepted only new, empty or marked `.ranikust-state`, never `/`, before its mode is set; `flock` on a descriptor of `/` in a container, whatever the state directory; delete through descriptors without following symlinks or crossing mounts, comparing the mount ID, since `st_dev` misses bind mounts of the same filesystem; keep the state directory (`/var/lib/ranikust` in a container) on the ignore list, never walked or deleted ([ADR-0022](../adr/0022-common-state-dir.md)) | [survey 02](../research/survey/02-kaniko-mode-requirements.md) §4; [experiment 09](../research/experiments/09-kaniko-mode-prototype.md) [measured]; red teams that end by checking no mount, process or build directory is left and nothing outside `builds/<id>` and `tmp/<id>` changed: `case_run_signal` (`tests/smoke/cases/stage-dirs/case.sh`, ADR-0061) and `rt_nothing_left` (`tests/vm/guest/quick.sh`, `--mode=auto` routing); the `flock` on `/`: T2c `a_second_build_on_slash_is_refused_by_the_lock` (`crates/ranikust/src/setup/tests.rs`) and, with two builds in one default container, `slash_lock_refuses_a_second_build` (`tests/vm/guest/quick.sh`, kaniko quick suite: the second is refused, exit 1, before its state directory exists; the first builds) [measured] |
| T5 | `RUN` escapes a bare `chroot` | A1, A2 | overlay, kaniko with mounts | `pivot_root`, or `MS_MOVE` + `chroot` only where seccomp refuses `pivot_root` with `EPERM` and `/` is a mount root, so `..` stops at the moved root ([Q-077](../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape)). Kaniko mode outside a container where root may not mount, where `RUN` would run in a bare `chroot` on the host, is refused ([ADR-0050](../adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md), Q-330); on `/` in a container `RUN` runs directly, the container being the boundary | [experiment 02](../research/experiments/02-run-sandbox.md) [measured]; the `MS_MOVE` case [measured]: the escapes of Q-077 (`chroot` then `..`, `/proc/1/root`) fail in `tests/vm/guest/smoke.sh`, as real root, in the `sysadmin` variant that enters its root with `MS_MOVE` and in the `privileged` one with `pivot_root`, and the `chroot` escapes work in a plain `chroot` (kernels 5.15 and 6.8, [Q-077](../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape)) |
| T6 | `RUN` keeps `CAP_SYS_ADMIN`, unmounts a bind and writes the real file; a setuid binary regains capabilities from a full bounding set | A1, A2 | overlay, kaniko with mounts | Drop the bounding and inheritable sets to Docker's 14 defaults for every `USER`, root included, before the `RUN`'s seccomp profile, which could fake the calls | [experiment 02](../research/experiments/02-run-sandbox.md) [measured]; `smoke_checks` in `tests/smoke/lib.bash` (`CapBnd` `a80425fb`, `CapInh` empty), T1 and T2; `rnk-sandbox` T2 `a_profile_cannot_fake_the_steps_before_it` |
| T7 | A descriptor ranikust holds (root, store, lock, socket) is inherited by `RUN` and opens a path outside the root | A1, A3 | all | `O_CLOEXEC` on everything; right after the clone, before any path the `RUN` chooses is looked up, the child closes every descriptor but stdio, its error pipe, its parent check, a network namespace, the stdio sources and a `#!` script (none a directory), so that neither a `WORKDIR` nor a `PATH` entry nor a mount target reaches one through `/proc/self/fd/<n>` (runc's CVE-2024-21626); an exec candidate reached through a magic link is refused, but a `#!` line naming `/proc/self/exe` still runs ranikust's binary in the sandbox (section 6; [ADR-0082](../adr/0082-full-sandbox-binds-the-binary-read-only-over-itself.md) binds the binary read-only over itself in the full sandbox, not built yet); a test that lists `/proc/self/fd` inside `RUN`, and T2 `red_team_an_inherited_descriptor_gives_a_run_no_way_out`. One exception: the sealed memfd holding a `#!` heredoc of a `Direct` `RUN` on `/`, which its interpreter must reopen as `/dev/fd/<n>`; it holds only that script and cannot be written ([ADR-0057](../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md)). Every `RUN`'s stdout and stderr are pipes of its own, copied by ranikust, never ranikust's stream descriptors, so a root `RUN` cannot truncate, rewrite or make non-blocking an operator's terminal or `> build.log`; the copy stops once the `RUN` is swept, whoever still holds a write end, and never blocks in a `read` a `RUN` reading its own pipe back could starve | [inferred]; `smoke_checks` in `tests/smoke/lib.bash` (at most stdio and `ls`'s own descriptors), T2c `red_team_direct_run_holds_only_its_own_descriptors` (`crates/rnk-sandbox/tests/direct_attacks.rs`), `case_run_direct_attacks` (`tests/smoke/cases/run/case.sh`) |
| T8 | A `RUN` process outlives a killed build and keeps writing to the root | A1, A6 | all | `PR_SET_PDEATHSIG(SIGKILL)` set from a thread that lives for the whole build, then a re-check that the parent is alive (the parent pid in the child's outer `/proc/self/stat`, since `getppid` is 0 in a new PID namespace). The full sandbox: that child stays as the `RUN`'s init, PID 1 of its PID namespace, which never changes its user or executes anything, so nothing clears its signal; it holds no capability, no handler and no descriptor of ranikust's, is not dumpable, and runs the command as PID 2; when ranikust dies the init dies and the kernel kills the namespace, a command that cleared its own death signal or executed a set-id binary included ([ADR-0080](../adr/0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md)). `Direct`: armed again after `setresuid`/`setresgid`, which clear it, and the sweep after each `RUN` (ADR-0051) | [experiment 02](../research/experiments/02-run-sandbox.md) [measured]; `rnk-sandbox` T2 `red_team_no_run_outlives_the_thread_that_cloned_it` (root, `USER 1000:1000`, a setuid and a setgid `sleep`, a `python3` clearing its own signal) and `the_run_is_pid_2_under_an_init_that_reports_its_status`; `tests/vm/guest/smoke.sh`, host variant: SIGKILL and SIGINT of ranikust during `RUN sleep 300`, and SIGKILL during a `RUN` that cleared its own death signal, leave no `RUN` process; `smoke_checks` in `tests/smoke/lib.bash` (the `RUN` is PID 2 under `ranikust-init`, which holds no capability) |
| T9 | Mounts made for a build propagate to the host or outlive the build | A1, A7 | overlay, kaniko with mounts | Enter a private mount namespace while still single-threaded and make `/` a slave; all build mounts live there and vanish with the process | [experiment 02](../research/experiments/02-run-sandbox.md) [measured]; `tests/vm/guest/smoke.sh` ends by checking no mount is left under its work directory, `rt_nothing_left` (`tests/vm/guest/quick.sh`) none below a build's directory, and `case_run_signal` (`tests/smoke/cases/stage-dirs/case.sh`) none after a cancelled build |
| T10 | Stub mountpoints or files ranikust adds (`resolv.conf`, `hosts`, fake `/proc`) leak into a layer or survive a crash; copying them changes the parent's mtime so `/etc` lands in every layer | A6 | all | Journal stubs in the state directory before creating them; remove them before the diff; restore the parent's metadata; clean up from the journal at startup, reading a journal only as a regular file of at most 16 MiB (pinned `O_PATH`, reopened `O_NONBLOCK`), so a FIFO or device node planted at its name is left with a warning, never blocked on or opened | [experiment 02](../research/experiments/02-run-sandbox.md), [experiment 12](../research/experiments/12-kaniko-mode-parallel.md) §4 [measured]; T0 `a_fifo_planted_as_a_journal_is_never_opened`, `a_fifo_planted_as_an_unpublished_journal_is_never_opened`, `a_journal_larger_than_any_run_writes_is_left_unread`, T2 `a_device_node_planted_as_a_journal_is_never_opened`; T0 `red_team_a_node_in_a_mount_target_is_enotdir_or_the_target_itself`, `red_team_stubs_swapped_for_links_out_or_nodes_are_left_with_the_host`, `red_team_planted_stub_records_stay_in_the_root`, T2c `red_team_a_block_device_in_a_mount_target_is_enotdir_or_the_target_itself`; `smoke_check_no_stub_leak` (`tests/smoke/lib.bash`: no stub in any layer), run by the smoke cases and the VM smoke suite |
| T11 | Registry credentials or build secrets reach logs, layers, the image config, cache keys, error messages, the `RUN` environment or a kernel keyring `RUN` can read | A3, A4 | all | Redact credentials in `Debug` and `Display`; give secrets only to the `RUN` that asks: as a read-only file (`0400`, root's, unless the mount asks otherwise), or with `env=` as a variable of that `RUN` alone, as BuildKit (ADR-0032); never on the command line; write a secret file only on the `RUN`'s own tmpfs, mounted by the child in its mount namespace, never on a disk (BuildKit's secret mount); keep values out of errors and `Debug` (`RunSpec`, `SecretFile`, `RunMounts`); give each `RUN` a new session keyring, and run every full-sandbox `RUN` under Docker's default seccomp profile, which refuses `keyctl`, `add_key` and `request_key` (root's user and persistent keyrings stay reachable under `--seccomp=unconfined`, a profile file that allows them, and in kaniko mode on `/`, where the container's profile decides; under the first two, a join an LSM refuses fails the `RUN`; [ADR-0109](../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)); send registry credentials over plain HTTP only to a registry the operator typed (`--insecure-registry`, or a destination under `--insecure`; `ClientOptions::http_credentials`, ADR-0036): a loopback registry is plain HTTP unasked (ADR-0032, as BuildKit) and a registry only `--insecure-pull` allows comes from the untrusted Dockerfile, so both get no credentials and a refusal names the flags, since a `RUN` shares the host network namespace and could sniff `lo` or serve the port; `localhost` resolves to loopback in ranikust's own requests; a credential goes only to the registry it was chosen for and the token realm that registry names: `oci-client` gets references built from `rnk_oci`'s parts, never re-parsed, and a request that would address another host fails before it is sent (ADR-0070); `--verbosity=trace` logs other crates at `info` at most, so a dependency's request internals never reach the log | [inferred]; `rnk-registry` tests `credentials_never_go_in_clear_to_an_unlisted_loopback_registry`, `an_insecure_registry_not_named_for_credentials_gets_none`, `oci_client_addresses_the_registry_credentials_were_chosen_for`, `a_reference_oci_client_would_address_elsewhere_is_refused`; `rnk-metrics` `dependencies_log_at_info_at_most`; the keyring: `rnk-sandbox` T0 `docker_default_loads_and_refuses` (`keyctl` refused in a forked child), T2 `a_run_gets_its_seccomp_profile` (the profile reaches a `RUN`; each `RUN` runs in a session keyring of its own, joined before the profile) [measured], T0 `an_eacces_on_the_keyring_join_fails_unless_the_profile_refuses_keyrings`, T2 `an_lsm_refusing_the_keyring_join_fails_a_run_that_could_reach_keyrings` |
| T12 | With `--parallel-stages`, one stage reads another stage's root and secrets | A4, A6 | kaniko without mounts | `--parallel-stages` refused (ADR-0060); a hardened `chroot` (Q-400 option b) only after a VM measurement | [experiment 12](../research/experiments/12-kaniko-mode-parallel.md) §3 [measured], [ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md) |
| T13 | A build poisons the shared store: a blob that does not match its digest, a half-written layer after a crash, a cache-key collision | A5, A6 | all | Verify digests when writing and reading; extract to a temporary directory and `rename` atomically; per-digest locks; versioned cache keys; no lower-trust principal writes a persistent store's state directory: kaniko mode on `/`, whose `Direct` `RUN` can, never opens one, never runs beside another build on its state directory (one `flock`, exclusive there, shared by persistent stores) and empties it at the end, and a persistent store refuses a directory holding `.ranikust-building` ([ADR-0077](../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md)) | [ADR-0005](../adr/0005-own-versioned-cache-key.md), [ADR-0006](../adr/0006-persistent-store.md) [inferred]; `rnk-store` T0 `a_kaniko_build_on_slash_leaves_nothing_its_run_wrote`, `a_kaniko_build_on_slash_and_other_builds_never_share_a_state_directory` |
| T14 | Cache mounts (`RUN --mount=type=cache`) are writable by every build that names them | A5 | all | Shared by design; give each trust domain its own store | [inferred] |
| T15 | Decompression bombs, huge or very many files, deep trees, fork bombs; a layer's privileged xattrs leaving a tree root cannot delete | A7 | all | Stream instead of buffering; fail cleanly on exhaustion; fork bombs are bounded only by the outer cgroup; a layer's `trusted.*` xattrs are never set, so filesystem metadata such as XFS's raw ACLs (`trusted.SGI_ACL_*`) never comes from an image ([ADR-0075](../adr/0075-layer-trusted-xattrs-are-never-set.md)); the kaniko-mode walk refuses an entry past `PATH_MAX`, as the overlay exporter does, so the paths it holds stay bounded: its memory per entry is bounded by `PATH_MAX` (about 4 KB for an entry's path in each directory task and each change), not by the entry's own name, so a tree of many entries near that depth still costs up to that much per entry; the copy walk shares one path buffer per side, linear in depth (`copying_a_deep_chain_takes_memory_linear_in_its_depth`) (red team `red_team_walking_a_deep_chain_takes_memory_linear_in_its_depth`); what extraction remembers of a layer follows what the layer leaves on disk, one entry per name, not its entry count or path depth; deletion and pruning keep no path per level, so a deep tree a `RUN` made costs memory linear in the entries on the walk's stack, no longer quadratic in its depth. What a registry or an image can make ranikust hold or wait for is bounded by the `--limit` limits ([ADR-0079](../adr/0079-resource-limits-with-a-limit-flag.md)): tar extension headers (`tar-header`, 1 MiB, refused at the header), manifests (`manifest`, 4 MiB, while they arrive), token responses and error bodies (`registry-body`, 1 MiB), image configs (`config`, 8 MiB, before the request), and silence from a registry (`registry-idle`, 2 min, which also frees the store's per-digest lock); a registry sending one byte just inside each `registry-idle` interval still holds a pull, and the store lock, for as long as the blob lasts ([Q-448](https://github.com/tran4774/ranikust/issues/28)) | [survey 04](../research/survey/04-risks-and-roadmap.md) §4 [inferred]; T0 `trusted_xattrs_from_a_layer_are_never_set`, heap bounds in `crates/rnk-rootfs/tests/memory.rs` [measured]; the XFS effect [inferred]; `rnk-rootfs` `extension_headers_are_bounded_by_the_tar_header_limit`, `rnk-registry` `limits_refuse_hostile_bodies_with_bounded_memory`, `rnk-builder` `a_stalled_blob_pull_releases_the_store_lock` |
| T16 | Hostile input makes ranikust panic; the release build aborts mid-build | A6, A7 | all | No panics reachable from input; fuzz the parser, the ignore matcher and tar extraction; cleanup that survives an abort (T8–T10) | [inferred] |
| T17 | `RUN` as uid 0 writes a kernel interface of a fresh `proc`: a sysctl such as `kernel/core_pattern` or `kernel/modprobe` (the host kernel then runs a `RUN` binary as host root) or `/proc/sysrq-trigger`; writing needs uid 0, no capability | A1, A2 | overlay, kaniko with mounts | Docker's read-only paths (`/proc/sys`, `sysrq-trigger`, `irq`, `bus`, `fs`) and masked paths (`kcore`, `keys`, `timer_list`, `/sys/firmware`…) after mounting `proc` and `sysfs`, found under those mounts by descriptor, never by name from the root; `proc` and `sysfs` go only on real directories of the root (a symlinked `/proc`, `/sys` or `/dev` is refused, T2 `red_team_a_symlinked_proc_keeps_its_kernel_files_protected`). Kaniko mode on `/` in a container where root may mount (`--privileged`), whose `Direct` `RUN` would see the host's devices and a writable `/proc/sys`, is refused, and so is one whose `/proc/sys` is writable without that (`systempaths=unconfined`, `procMount: Unmasked`) ([ADR-0054](../adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md)). Host devices passed with `--device` stay readable and writable by a `RUN` there: the operator's responsibility, which ranikust cannot see | host-safety review of `rnk-sandbox`, moby `daemon/pkg/oci/defaults.go` [inferred]; `smoke_checks` in `tests/smoke/lib.bash` (`/proc/sys` read-only) and a `/proc/sys/kernel/domainname` write that fails (`tests/vm/guest/smoke.sh`) |
| T18 | A device node opens a host device: one `RUN` creates with `mknod` (`CAP_MKNOD` is a Docker default) or one from a layer, with no device cgroup to stop it; or `RUN` changes the mode of a host node bound into `/dev` | A1, A2 | overlay, kaniko with mounts | `nodev` on the root's bind, every `RUN --mount` bind and the `/dev` tmpfs; the `/dev` nodes are read-only binds of the outer ones, so I/O works and `chmod` or `chown` fails. A remounted bind is remounted through its own descriptor (`open_tree` + `move_mount`), never found again by a name whose symlinks the bind could change; where those calls are refused, its target may not pass through a symlink, and the mount found again must show the source's inode on a mount other than the one below and not one the child bound before (their mount ids are recorded), or the `RUN` fails; every mount below the root's bind is `nodev` too (`mount_setattr(AT_RECURSIVE)`, or a root with a submount refused on kernels before 5.12). Images: with the full sandbox (overlay mode, kaniko mode off `/`) `COPY` keeps device nodes as BuildKit, so an image may hold one copied in; only kaniko mode on `/` (`Isolation::Direct`, no `nodev`) leaves them out of `COPY` as of extraction (ADR-0041, ADR-0060) | host-safety review of `rnk-sandbox` [inferred]; T2 `a_mount_below_the_root_is_nodev_in_the_run`, `red_team_a_symlinked_mount_target_keeps_its_bind_nodev`; smoke checks of `tests/smoke/lib.bash` (root and `/dev/null` mounts, `nodev` and read-only) and a `mknod` node that does not open (`tests/vm/guest/smoke.sh`) |
| T19 | `RUN` uses the operator's terminal: reads keystrokes, or pushes input into the operator's shell with `TIOCSTI` | A1, A3 | all | `setsid` before `execve` (no controlling terminal), in the full sandbox the init's before the `RUN`'s seccomp profile, which could fake it; stdin `/dev/null` by default, as Docker | host-safety review of `rnk-sandbox` [inferred]; `smoke_checks` in `tests/smoke/lib.bash` (`! test -t 0`), T1 and T2; `rnk-sandbox` T2 `a_profile_cannot_fake_the_steps_before_it`, T0 `a_faked_setsid_fails_nothing_and_a_refused_one_fails_the_run` |
| T20 | A `RUN` reaches network-configuration code of the kernel with capabilities over its network namespace: without `CAP_NET_ADMIN`, `--allow network.none-userns` puts a `--network=none` `RUN` in a network namespace owned by a user namespace of uid 0, where its root holds every capability, so it could reach nf_tables, x_tables, xfrm, IPVS or traffic control, or join that user namespace through `ioctl(NS_GET_USERNS)` and `setns` | A1, A7 | overlay, with the flag | Refused by default. With the flag, a seccomp filter installed after every mount, before the `RUN`'s profile, which cannot fake its install, and under `--seccomp=unconfined` too, refuses `setns`, `unshare`/`clone` with `CLONE_NEWUSER`, `clone3` (`ENOSYS`), `io_uring`, `socket(AF_KEY)`, netlink other than route, sock_diag, audit and uevent (`EPROTONOSUPPORT`), the x_tables, arp_tables, ebtables, IPVS, IPsec policy and multicast-routing socket options of `SOL_IP` and `SOL_IPV6` (not `IPV6_RECVTCLASS`/`IPV6_TCLASS`, which share ip6_tables' numbers), `TCP_REPAIR`, and syscalls of another architecture. Residual: every other `CAP_NET_ADMIN`-gated interface in that namespace stays open, rtnetlink first, including `ip link add type <kind>` loading `rtnl-link-<kind>` modules into the host kernel (ADR-0033) | `rnk-sandbox` T0 `the_filter_refuses_what_it_should_and_nothing_else`; T2 `a_profile_cannot_fake_the_steps_before_it` (a profile faking `seccomp` leaves the filter on); VM smoke `tests/smoke/netns-check.py`, over the default profile and without it |
| T21 | In kaniko mode on `/` (no mounts, one PID namespace, `RUN` as uid 0) a `RUN` reads or tampers with ranikust's state (pulled and encoded blobs), its descriptors, environment or memory through `/proc/<pid>/`, or leaves a process that does so during the walk, the export or the push; or plants a link where ranikust opens a path after it (an output, a `--secret` file), so that ranikust opens it with an authority the `RUN` lacks | A3, A5, A6 | kaniko without mounts, on `/` | ([ADR-0051](../adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)) No path for state: blobs, encoded layers and kept stages (as tars) are anonymous files on disk (`O_TMPFILE` in the state directory, else a name unlinked at once while no `RUN` process exists, only where the unlink removes the name: a state directory on NFS, FUSE, SMB or AFS, a file still linked after its unlink, or an unlink refused, fails the build (`Error::NameOutlivesUnlink`), behind the store's fence, poisoned by a `RUN` that ended with an error and never used after a build cut short in this container (a marker naming its PID namespace and boot; a `RUN` can delete it, so this covers accidental kills only; one build per state directory, by `flock`, persistent stores included, ADR-0077), ADR-0056 and Q-360, ADR-0066; the state directory never a persistent store and emptied of what a `RUN` left at the end (ADR-0077); the state directory's identity checked after each `RUN`; `memfd` only when no file can be made on disk), held by descriptor, the state directory holding no name while a `RUN` runs; ranikust is not dumpable (`PR_SET_DUMPABLE 0`, first in `main`, and a soft `RLIMIT_CORE` of 0), so `/proc/<ranikust>/{fd,environ,mem,maps,root}` need `CAP_SYS_PTRACE`; the `RUN` child drops every capability outside Docker's defaults from its bounding set, and the `RUN` is refused when any such capability remains without `CAP_SETPCAP` to drop it (`SYS_ADMIN` could mount over `/proc`, `BPF` with `PERFMON` read kernel memory, `SYS_MODULE`, `SYS_RAWIO`); the child always clears its ambient and inheritable sets and keeps only Docker's defaults in its permitted and effective sets; ranikust itself keeps no more ([ADR-0078](../adr/0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md)): first in the set-up, single-threaded, its own bounding, ambient, inheritable, permitted and effective sets are cut the same way, so whatever it opens on `/` after a `RUN` the `RUN` could open itself, with one exception: a link a `RUN` plants at an operator path to ranikust's own `/proc/self/fd/<n>`, which no ptrace check guards for ranikust itself, opens one of ranikust's own descriptors (no gain beyond the build's own outputs; closed by opening operator paths without following links); every `--secret` is read once while the build is prepared, never opened by path again; ranikust is a child subreaper and, after each `RUN`, kills the `RUN`'s process tree through pidfds after checking each process's parent and start time again (no pid reuse), and reaps by pid without blocking; the `RUN` environment is the Dockerfile's only; registry credentials go only over verified TLS (`ClientOptions::verified_credentials_only`), since a `RUN` may edit `/etc/hosts` or `/etc/resolv.conf`; kaniko mode on `/` where root may mount, or whose `/proc/sys` is not read-only (`statvfs`), is refused ([ADR-0054](../adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md)); where seccomp refuses `pidfd_open`, only ranikust's own children get a plain `kill` (their pids are not reused before ranikust reaps them), deeper processes on later rounds once reparented. Not closed, warned: `/proc/<pid>/cmdline` (a `--build-arg` value), another process holding the credential variables or a `--secret` or `--build-arg` read from the environment (a CI shell), files mounted into the container, a `--secret src=` file (ADR-0084: a `RUN` may copy it into the image, the Dockerfile author's choice). Not closed: anonymous requests (and any request to a plain-HTTP or unverified registry) can still be redirected by a `RUN` that edits the resolver files. No secret is ever written to `/` for a `RUN` there: a secret as a file and `--network=none` are refused, a cache mount left out with a warning (what the `RUN` writes at its target, a tool's tokens included, then lands in the layer: the warning says so), a secret as a variable given; a `#!` heredoc runs from a sealed memfd, never a named file ([ADR-0057](../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md)) | VM `secrets` suite (default Docker, `--cap-add SYS_PTRACE`, started from `sh -c`; V11 ranikust's capabilities, V12 a swapped `--secret` file), T0 `a_file_secret_swapped_after_the_start_changes_nothing`, `a_multi_threaded_process_is_refused_the_trim`, T2 `a_direct_run_leaves_no_process_behind`, `a_direct_run_on_slash_keeps_only_dockers_capabilities` [measured]; T0 `credentials_are_withheld_from_plain_http_and_unverified_tls_as_configured`, `verified_credentials_only_withholds_them_from_plain_http`, `a_refusal_after_credentials_held_back_for_kaniko_mode_says_why`, `verified_credentials_only_tells_how_to_reach_an_unlisted_loopback_over_https` (credentials only over verified TLS) [measured] |
| T22 | The build context's owner redirects ranikust's own host reads and writes: a symlink committed at an output path or a parent of one (root truncates or writes a host file), a `--dockerfile` or `--context` path through a committed symlink (a host file read as the Dockerfile, a root-only directory copied into the image), a root-only file hardlinked into the context (`fs.protected_hardlinks=0`) | A1, A3 | all; kaniko on `/` also through `RUN` (T21) | Outputs reached from `/` one name at a time, never through a symlink and never into the context, written by descriptor; digest files only into a regular file with one link; `--dockerfile` read through the context's `RootPath` once its path enters the context; `--context` opened with `RESOLVE_NO_SYMLINKS`; a context file with other links and another owner than its directory's, or one its directory's owner could not read by its mode bits refused for `COPY`, `ADD`, the Dockerfile and ignore files, judged against the directory it was found in ([ADR-0076](../adr/0076-the-build-context-owner-is-a-lower-trust-principal.md)) | `rnk-rootfs` `host::tests`, `a_context_file_its_directory_owner_could_not_read_is_refused`, `the_context_rule_judges_a_file_by_the_directory_it_is_in`; `rnk-context` `a_context_path_through_a_symlink_is_refused`, T2c `a_root_only_file_hardlinked_into_the_context_is_never_read`; `ranikust` `digest_files_never_follow_a_planted_name`, `outputs_in_the_build_context_are_refused`, `a_dockerfile_in_the_context_never_follows_a_link_out_of_it`; `rnk-builder` `an_output_never_goes_through_a_symlinked_directory`, T0 [measured] |
| T23 | A binary that is not the reviewed source of a tag is published or attested as a ranikust release: a branch or pull request of the repository runs a modified workflow that attests a file; a poisoned Actions cache entry or tool archive enters the release build; a tag pushed before the `release` environment exists publishes without approval; a user checks a download with `gh attestation verify --repo` alone, which accepts any workflow on any ref | A8 | release | Only the maintainer creates, moves or deletes `v*` tags (`release tags` ruleset) and changes reach `main` only through a pull request with CI passed (`main` ruleset); the release is built from the tag, on main, on a GitHub-hosted runner, with `--locked` and no cache (Zig downloaded with a pinned SHA-256, cargo-zigbuild checked by its installer); the attestation is made in a job of its own, so the build never holds the signing token; the workflow stops a tag unless the `release` environment exists with a required reviewer and only the tag rule `v*`, checked before the build and again before publishing; users verify with `SHA256SUMS` and `gh attestation verify --signer-workflow tran4774/ranikust/.github/workflows/release.yml --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners` (GitHub CLI 2.102.0 or later) ([ADR-0089](../adr/0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md), [ADR-0092](../adr/0092-a-release-is-verified-by-workflow-and-tag-and-built-without-cache-or-unprotected-environment.md)). Residual: agents work with the maintainer's GitHub token, so the tag ruleset and the environment approval bind them only through their ask rules ([ADR-0087](../adr/0087-work-flows-through-github-issues-pull-requests-and-a-project.md)), not through GitHub's permissions. GitHub itself, its runners and its attestation service are trusted | [code] `.github/workflows/release.yml`; `check-environment` refusing a missing or unprotected environment and the strict command in the tarball's note, T0 (`tests/release/test_release.py`); the static-pie check with a flooding fake `readelf`, T0 (`tests/release/check-static-test.sh`); the pull-request dry run builds, checks and packages [measured]; the tag path (attestation, approval, draft) runs first with the first release [inferred] |

## 6. Accepted risks

These follow from decisions and are documented to users rather than fixed:

- `RUN` runs as root inside its sandbox, and the sandbox is about as strong
  as `docker build` ([ADR-0003](../adr/0003-dockerfile-trust-and-default-isolation.md)).
- Until `execve` the `RUN` child is a copy of ranikust, so a command whose
  `#!` line names `/proc/self/exe` (or a symlink a parallel stage swaps in
  a shared cache between the child's check and `execve`) runs ranikust's
  own binary inside the sandbox, as runc's CVE-2019-5736. What holds today:
  ranikust is non-dumpable, so no other `RUN` process opens that copy's
  `/proc/<pid>/exe`; its command line refuses the script path and exits;
  and a running binary cannot be opened for writing (`ETXTBSY`) while
  ranikust runs, a kernel behaviour that a future kernel may drop.
  [ADR-0082](../adr/0082-full-sandbox-binds-the-binary-read-only-over-itself.md)
  decides that the full sandbox binds the binary read-only over itself (not
  built yet) and that `Direct` on `/` keeps these barriers, where a `RUN`
  already shares the container with ranikust.
- A full-sandbox `RUN`'s init (PID 1, ADR-0080) is a copy of ranikust: its
  argument and environment strings are zeroed before the command starts, so
  `/proc/1/cmdline`, which has no ptrace check, shows no `--build-arg`
  value or `--secret` path, and `/proc/1/environ` stays closed (the init is
  not dumpable). A root `RUN` can slow its init's death when ranikust dies,
  but not stop it: as a process of the same uid it may make the init
  `SCHED_IDLE` and pin it to one CPU without `CAP_SYS_NICE`, then keep that
  CPU busy. Measured in the VM (kernel 6.8, EEVDF), the namespace died
  0.9 to 1.8 ms after the death signal fired with no such tricks, 7.6 s
  with 8 spinners pinned to the init's CPU and 30.6 s with 32: about 1 s per
  spinner **[measured]** (`red_team_a_run_can_slow_its_inits_death_but_not_stop_it`),
  so the delay is bounded only by the processes the `RUN` may start (the
  outer cgroup's `pids.max`), during which its processes keep running. An
  init under a uid of its own would not help: a root `RUN` holds
  `CAP_SETUID` and switches to that uid **[measured]**. A cgroup for the
  command apart from the init's, killed with `cgroup.kill` as a second line
  where a writable, delegated cgroup exists, or a user namespace that does
  not map the init's uid (`--userns`, Phase 3) closes it: decided in
  [ADR-0083](../adr/0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md),
  not built yet; until then this is the state.
- kaniko mode inside a container that forbids mounts has no isolation
  between `RUN` and the container ([ADR-0002](../adr/0002-two-snapshot-modes.md)).
  ranikust keeps its own state and credentials out of `RUN`'s reach (T21,
  ADR-0051), except what it can only warn about: a value on its command
  line (`/proc/<pid>/cmdline` is readable whatever `PR_SET_DUMPABLE` says),
  credentials another process of the container holds in its environment
  (a CI shell that started ranikust: run ranikust as the container's PID 1),
  and any file mounted into the container (a `config.json`, a
  service-account token: pass registry credentials in `DOCKER_AUTH_CONFIG`).
  A root `RUN` can still list the descriptor numbers of
  `/proc/<ranikust>/fd`, whose entries a non-dumpable process gives to root,
  but neither read their targets nor open them; before Linux 6.0 it can
  also read `/proc/<ranikust>/fdinfo/<n>` (position, flags, inode, mount
  id, sizes; no contents). The build context, `.dockerignore`d files such
  as `.env` or `.npmrc` included, every `--secret src=` file and a mounted
  `config.json` are files on `/` like any other: a `RUN` reads them; pass
  secrets and credentials in the environment of ranikust started as PID 1.
  ranikust reads each `--secret` once, at the start, and warns for each
  file (ADR-0078). A `RUN` that reads such a file can copy it into the image
  or anywhere else: ranikust does not stop it, and doing so or not is the
  Dockerfile author's decision
  ([ADR-0084](../adr/0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md)).
  ADR-0084 also decides that a secret as a file for a `RUN`, refused on `/`
  today, is allowed: its own path, not its directory, is ignored by the
  snapshot, and the file placed for a `RUN` is removed after it
  ([ADR-0085](../adr/0085-a-secret-file-on-slash-goes-at-its-target-is-ignored-by-its-own-path-and-removed-after-its-run.md); decided, not built yet).
  A `RUN` may edit `/etc/hosts` and `/etc/resolv.conf` (bind mounts the
  ignore list keeps) and so redirect ranikust's later requests: credentials
  go only over verified TLS there, but an anonymous pull of a tag, or any
  request to a plain-HTTP or unverified registry, can reach a host of the
  `RUN`'s choosing. With `kernel.yama.ptrace_scope` 0 a `RUN` process may
  trace other dumpable processes of its user (ranikust warns). Nothing of ranikust's
  state outlives a kaniko-mode build on `/`: its state directory is never a
  persistent store, is shared with no other build, and is emptied of what a
  `RUN` wrote there at the end ([ADR-0077](../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md)).
  The store writes its marker back after every `RUN` and at the end, and a
  `RUN`'s `SIGINT` or `SIGTERM` cannot stop ranikust at once (only the
  kernel, a process outside the container's PID namespace or ranikust's
  parent can): it cancels the build, which still closes its store.
  Not closed: a `RUN` can still end ranikust before its end, by `SIGKILL`
  where ranikust is not PID 1, or through the OOM killer after raising its
  `oom_score_adj`. If it also removed `.ranikust-building` in between, what
  it wrote stays, a forged store marker included, and nothing in ranikust
  can stop that. So the rule is load-bearing: a state volume used by kaniko
  mode on `/` must never later serve overlay-mode or per-stage builds, and
  it must be mounted at `--state-dir` itself, never at a parent a `RUN`
  could rename it in. Sharing one safely needs the store's entries checked
  against a key no `RUN` can reach ([open: #30]).
- Builds one after the other in one container trust one another. A later
  build empties `/` before its first base when an earlier build left its
  marker, `/ranikust/.built` (ADR-0052, D1), so files left by accident never
  reach its image; but a `RUN` of the earlier build runs as root on `/` and
  can delete the marker, or replace the `ranikust` binary under
  `/ranikust`, which no build empties. Run each untrusted Dockerfile in a
  fresh container.
- Under `--seccomp=unconfined` (or a profile file that allows as much),
  ranikust installs no seccomp filter on a `RUN` but
  [ADR-0033](../adr/0033-network-none-loopback-through-a-user-namespace.md)'s,
  which belongs to a `--network=none` `RUN` under `--allow network.none-userns`
  and refuses neither `AF_VSOCK` nor `AF_ALG` ([ADR-0003](../adr/0003-dockerfile-trust-and-default-isolation.md)).
  Such a `RUN` in the full sandbox can then open `AF_VSOCK` sockets
  (the host/guest transport, global across network namespaces unless the
  host opts in **[external]**) and `AF_ALG` sockets (the kernel's crypto
  API), and, where the host lets a process without `CAP_SYS_ADMIN` create a
  user namespace (no outer seccomp profile or AppArmor restriction in the
  way, `user.max_user_namespaces` above 0 **[external]**,
  `namespaces(7)`, [experiment 03](../research/experiments/03-privilege-matrix.md)),
  create its own and hold every capability in it over the namespaces it
  makes, which opens the kernel interfaces T20 lists; a root `RUN` can also
  use uid 0's user and persistent keyrings (T11). The `--network=none`
  `RUN` under `--allow network.none-userns` is the exception for user
  namespaces: its filter refuses `CLONE_NEWUSER` (T20). A `RUN` in kaniko
  mode on `/` has no filter of ranikust's either; the container's own
  profile applies, whatever it is, and `--seccomp` is refused there.
  Otherwise the default profile applies ([ADR-0109](../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)): moby/profiles seccomp
  v0.2.4's, as BuildKit `cd8ee24` vendors it
  (`vendor/github.com/moby/profiles/seccomp/default.json`) **[external]**.
  It leaves `AF_ALG` and `AF_VSOCK` out of the socket domains it allows,
  in a recent change (its comment on `AF_VSOCK` mentions Linux 7.0), allows
  `unshare` and `clone` with `CLONE_NEWUSER` only with `CAP_SYS_ADMIN`, and
  refuses the keyring calls. Whether the Docker release you run has the same
  profile is not checked here. ranikust compiles it with its own code, so the
  filter is only as right as that compiler, which T0 tests check against a
  table of moby's profile and in forked children.
- A `--seccomp` file may answer a call with a faked success (errno 0). Every
  set-up step that need not follow the profile comes before it (ADR-0109),
  and a faked fork of the init fails the `RUN`, but the other steps after
  it trust its answers. For example, a faked `capset`, `setsid` or `USER`
  switch leaves the `RUN`'s init with its capabilities, the command in the
  init's session (which has no terminal either) or the command as root
  instead of its `USER`; a faked `read` of the go pipe starts the command
  before its init has dropped its capabilities and descriptors (the init
  is not dumpable, so the command cannot reach them); a faked `close` can
  leave the `RUN` waiting forever; a faked `openat2` in the `PATH` search
  closes the command's stdin. The bounding set, the keyring, ADR-0033's
  filter, the init's dump flag and the umask stay as they were; Docker's
  default profile fakes none of these calls.
- Until `--userns` (Phase 3), a distinct host account whose uid and primary
  gid are the `RUN`'s can reach a non-root `RUN` while it lives
  ([ADR-0047](../adr/0047-kaniko-workdir-chain-is-private.md), corrected by
  [ADR-0074](../adr/0074-setuid-through-proc-root-of-a-full-sandbox-run-is-nosuid.md)). Both ids must match: a `USER <uid>` with no passwd entry runs
  as gid 0, so an account `<uid>:<uid>` is refused, and a uid match alone
  lists only the descriptor numbers in `/proc/<pid>/fd` **[measured]**
  (2026-11-03, kernels 5.15 and 6.8). In the full sandbox the `RUN` keeps the
  Dockerfile's raw uid and gid in the user namespace ranikust runs in (no
  `CLONE_NEWUSER`) and, once it has executed, is dumpable, unless its binary
  is exec-only (`0711`), which makes it non-dumpable and then nothing is
  reachable **[measured]** (2026-11-03, kernels 5.15 and 6.8). So such an
  account passes the kernel's check for `/proc/<pid>/environ` (every `env=`
  secret of that `RUN`), `/proc/<pid>/root`, `/proc/<pid>/cwd` and
  `/proc/<pid>/fd/*`: all ask for matching ids and a dumpable target
  (`PTRACE_MODE_READ_FSCREDS`; `ptrace(2)`, `proc_pid_environ(5)`,
  `proc_pid_root(5)`, `proc_pid_cwd(5)`, `proc_pid_fd(5)`) **[external]**.
  Through the root and the descriptors it reads and writes what that uid may
  in the root being built, a secret file mounted with `uid=` that uid, and a
  cache mount that uid owns (T14); files only root may read (`0600`) stay
  unreadable. With `kernel.yama.ptrace_scope` 0 it can also attach with
  `ptrace` and read and write `/proc/<pid>/mem`, which are
  `PTRACE_MODE_ATTACH` checks that Yama does not restrict at 0 (`ptrace(2)`,
  `proc_pid_mem(5)`) **[external]**;
  ranikust's warning for that setting (`crates/rnk-sandbox/src/sweep.rs`)
  covers only one `RUN` process attaching to another. Before its command
  runs, the child is a copy of ranikust's memory: it is made non-dumpable
  again right after its uid switch, so only the one system call between
  `setresuid` and that `prctl` is open, and only where
  `fs.suid_dumpable` is 1 (the common defaults are 0 and 2)
  **[inferred]**. The account gains
  nothing over the state directory, which is root's `0700`: any host user can
  read the `RUN`'s `/proc/<pid>/mountinfo` where `hidepid` is 0, but it shows
  only the names of the state-directory and overlay layer paths
  **[measured]** (same runs). It gets no privilege from a setuid file either: the `RUN`'s
  mounts belong to its mount namespace, which the kernel treats as `nosuid`
  for a process of another one (the same file gives `euid=0` for the account
  in its own namespace, and the `RUN`'s root mount has no `nosuid` flag), and
  a hardlink out of that root fails with `EXDEV`, since the two paths are on
  different mounts, whatever `fs.protected_hardlinks` is **[measured]**
  (2026-11-03, kernels 5.15 and 6.8, full sandbox in overlay mode). Docker
  without user-namespace remapping behaves the same **[inferred]**. A `RUN`
  as root is reachable only by host root, who is trusted; in kaniko mode on
  `/` the `RUN` is an ordinary process of the container, and the container
  runtime's boundary applies (section 3).
- `RUN` shares the host network by default
  ([ADR-0010](../adr/0010-run-network-host-or-none.md)), as kaniko's does; it
  can reach services that listen on the host and the cloud metadata
  service (`169.254.169.254`, `fd00:ec2::254`), which hands out the node's or
  pod's cloud credentials. The maintainer accepted this on 2026-10-31
  rather than block the metadata addresses by default: ranikust does not
  filter `RUN`'s traffic. Mitigate outside ranikust: a network policy that
  denies the metadata addresses to the build pod, IMDSv2 with a hop limit
  of 1 so a container cannot reach it, workload identity instead of node
  credentials, or `--network=none` for builds that need no network (it keeps
  the `RUN` off them; see the next item).
- `--network=none` isolates the `RUN` only. ranikust's own registry
  requests still leave from the network namespace ranikust runs in. That
  includes the anonymous token realm and the redirects a registry names. A
  registry named by the Dockerfile can therefore make ranikust send a `GET`
  to any http(s) URL, internal addresses and the metadata service included.
  A `200` body with a JSON `token` or `access_token` of printable ASCII is
  accepted as a token and sent back to that registry as its bearer token
  **[inferred]**, and a 200-character excerpt of an error body can reach the
  build's error message. The maintainer
  accepted this on 2026-11-02 until a fix is chosen [open: #25]. Without
  `--network=none` a `RUN` reaches the same addresses anyway (ADR-0010).
- The shared network namespace also holds the abstract `AF_UNIX` sockets
  bound in it: network namespaces isolate them (`network_namespaces(7)`)
  **[external]**, and no file permission guards them. A default `RUN`, and a
  `RUN` in kaniko mode on `/`, which has no namespace of its own at all,
  connects to those of the namespace ranikust runs in (the host's, when it
  runs on a host), where a service sees the `RUN`'s uid through
  `SO_PEERCRED` (`unix(7)`) **[external]**: 0, or the `USER`'s raw uid
  **[inferred]**. This is part of the shared network that ADR-0010 accepts;
  builds running at once on one host share the namespace too.
- A registry listed with `--insecure-registry`, or a destination under
  `--insecure`, gets its credentials over plain HTTP, which a `RUN` sharing
  the host network could observe on the path; kaniko (go-containerregistry)
  does the same for its insecure registries. Unlisted loopback registries
  and registries only `--insecure-pull` allows are plain HTTP without
  credentials (T11, ADR-0036). `--skip-tls-verify*` skips verification for
  everything sent to that registry, its token service included, so a
  credential exchanged there goes to whoever answers. The `oci-client` requests resolve `localhost` with the
  system resolver; only ranikust's own requests pin it to loopback.
- Some environments get no mode at all and are refused
  ([ADR-0004](../adr/0004-support-every-privilege-level.md)).

## 7. Not verified yet

From [survey 00](../research/survey/00-summary.md) and
[survey 04](../research/survey/04-risks-and-roadmap.md) §4; each needs a test
before 1.0:

- the host-safety attack suite: path traversal and symlink or hardlink
  escapes in layers, `RUN mount`, `setsid sleep 1000 &`, a fork bomb under a
  cgroup limit, writes to `/etc/resolv.conf`, a `--state-dir` at `/`,
  below `/home/<user>` or in a directory others may write, each followed by a check that the host is unchanged
  and no mount or process is left;
- races during extraction and deletion, and concurrent builds on one store,
  including a crash halfway;
- `--userns` end to end;
- git contexts (Phase 5, ADR-0035): an untrusted repository or context
  argument must not run code on the host through the external `git`
  (hooks, `core.fsmonitor`, `ext::` and `file://` submodules, a ref or URL
  starting with `-`, host configuration read unasked), and a clone is read
  only through `RootPath`;
- overlay stage roots as real root (T2 VM, `rnk-snapshot`'s
  `tests/overlay.rs`): mounting, unmounting and releasing leave no mount
  under the state directory, and the read-only view refuses writes. Not
  yet tested: an upper directory a hostile `RUN` filled (planted symlinks,
  FIFOs and devices, deep trees) read by the exporter, whose reads go
  through `RootPath` without following or opening them; a stage released
  while a `RUN` still holds files open under its mount (`MNT_DETACH`);
- the `RUN` sandbox as real root (T2 VM): T18 for a layer's device node
  and `chmod /dev/null`. Checked by `just test-vm
  smoke` on kernels 5.15 and 6.8: T5's `chroot` escapes under `MS_MOVE`
  (Docker's seccomp profile) and `pivot_root`, T17 (a `/proc/sys` write
  fails), T18 for a node the `RUN` makes with `mknod`, T19;
- real Kubernetes (containerd's AppArmor profile, `procMount`,
  `hostUsers: false`, Pod Security Standards) and SELinux.

## 8. Rating findings

| Severity | Examples |
|---|---|
| Critical | Untrusted input makes ranikust write or delete a host file outside its directories (T1–T4); `RUN` reaches the host from a mode that promises isolation (T5, T6, T9) |
| High | Credential or secret leak (T11); descriptor or capability leak into `RUN` (T7); store or cache poisoning across builds (T13) |
| Medium | Orphan processes or leftover stubs and mounts (T8, T10); denial of service from untrusted input with default settings (T15, T16) |
| Low | Host metadata such as the hostname or timestamps leaking into image content |

Anything listed in §6, or anything that needs the operator to pass a
dangerous flag knowingly, is not a vulnerability; it may still be a
documentation bug.
