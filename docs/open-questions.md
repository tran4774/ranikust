# Open questions

Since 2026-11-03 open design questions live on GitHub, not here
([ADR-0088](adr/0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md)):
each is an issue labelled
[`question`](https://github.com/tran4774/ranikust/issues?q=is%3Aissue+label%3Aquestion),
on the [ranikust Project](https://github.com/users/tran4774/projects/1).
This file keeps the [Answered](#answered) archive, which ADRs and design
documents link to; nothing new is added to it.

## How to use it

- **Raising.** Do not guess an undecided question. Open an issue with the
  **Design question** form (Context, Options, Recommendation, Interim,
  Blocks, Area), titled by the question alone: a new question takes no
  `Q-NNN` number, its issue number `#N` names it. Labels: `question`, the
  `crate:<name>` labels of its area and `needs-decision` when it waits on
  the maintainer. Mark the place that needs the answer with `[open: #N]`,
  and continue with work that does not depend on it, or with the smallest
  reversible choice, recorded under "Interim" in the issue.
- **Answering.** The maintainer answers in an issue comment. The decision is
  recorded where it belongs (a new ADR, a design doc, a crate card) by a pull
  request that says `Closes #N` and removes the `[open: #N]` markers;
  `just docs-lint` checks that no marker names a closed issue.
- **Migrated.** The 29 questions open on 2026-11-03 became issues #1–#31
  (with gaps); their titles keep their `Q-NNN:` prefix, so the ADRs and
  this archive that name them still resolve.

## Answered

### Q-422: How is ranikust's own binary kept out of a `RUN`'s reach before `execve`?

- **Answer (2026-11-03):** "Q-422: (b) cho Full sandbox, (d) cho Direct
  trên /": option (b) for the full sandbox: ranikust binds its own binary
  read-only over itself in the build's mount namespace, as runc 1.2 does;
  option (d) for `Direct` on `/`: the barriers below stay and the residual
  is documented. Options (a) and (c) are not taken. The implementation is a
  follow-up. Recorded in
  [ADR-0082](adr/0082-full-sandbox-binds-the-binary-read-only-over-itself.md).
- **Blocks:** nothing yet; **Area:** `rnk-sandbox`, security (T7)
- **Context:** until `execve`, the `RUN` child is a copy of ranikust, so
  `/proc/self/exe` is ranikust's binary on the host. The child refuses an
  exec candidate reached through a magic link, but the kernel opens a
  script's interpreter itself: a command whose `#!` line names
  `/proc/self/exe` runs ranikust's binary in the sandbox, with the script
  as its first argument (runc's CVE-2019-5736 vector), and so does a
  symlink swapped between the child's check and `execve` (a cache a
  parallel stage writes). Barriers today, pinned by tests: ranikust is
  non-dumpable from its first line, so another process of the `RUN`
  cannot open `/proc/<pid>/exe` of that copy; its command line refuses the
  script path as a subcommand and exits 2 (quick case `self-exe`); and the
  kernel refuses to open a running binary for writing, `ETXTBSY`
  (`a_running_binary_cannot_be_opened_for_writing`), which holds while
  ranikust runs. An `ETXTBSY` that a future kernel drops would leave only
  the first two.
- **Options:** (a) at start-up, copy the binary into a sealed memfd and
  re-exec from it, as runc does: `/proc/self/exe` then names a file no one
  can write; costs one copy of the binary (11.6 MiB, static musl, x86_64)
  in memory for the process's life and one `execve` at start-up, a few
  milliseconds; (b) bind the binary read-only over itself in the build's
  mount namespace, as runc 1.2 does with an overlay: one mount at start-up
  (well under a millisecond, no copy), only where ranikust may mount,
  which is where the full sandbox runs; `Direct` on `/` would stay as
  today; (c) a small static trampoline the child execs first, which then
  runs the command: one more `execve` per `RUN` (about a millisecond) and a
  second binary to ship or embed, with no change at start-up; (d) keep the
  barriers above and document the residual.
- **Recommendation:** (b) where ranikust may mount, with (d) for `Direct`
  on `/`, where a `RUN` already shares the container with ranikust;
  (a) if one mechanism should cover every mode.
- **Interim:** (d): the barriers above, with the tests that pin them.
- **Raised:** 2026-11-03

### Q-445: Should kaniko mode on `/` refuse `--secret` files?

- **Answer (2026-11-03):** "Bật lại --secret. Bỏ qua thư mục này khi build
  (ignore). Còn việc câu lệnh RUN của dockerfile có copy nó đi đâu hay không
  thì đó là quyết định của người dùng. Nên ghi cảnh báo vào tài liệu.":
  option (b), refusing file secrets, is not taken. In kaniko mode on `/`
  `--secret` files work: the directory that holds the secret files is put on
  the snapshot ignore list so that none reaches a layer, which lifts two
  refusals (the start-up refusal of a `--secret src=` file that lies in the
  root outside the ignore list, and ADR-0057's refusal of a `RUN
  --mount=type=secret` as a file); whether a `RUN` copies the file anywhere
  is the user's decision, and the book and the threat model warn that every
  `RUN` on `/` can read it. The implementation is a follow-up, and which
  directory, where the file is placed and its removal are
  [Q-453](#q-453-where-does-kaniko-mode-on--put-a-secret-file-and-which-directory-does-the-snapshot-ignore).
  Recorded in
  [ADR-0084](adr/0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md).
- **Blocks:** nothing yet; **Area:** `ranikust`
- **Context:** in kaniko mode on `/`, a `--secret src=` file is a file on
  the `/` every `RUN` shares, so any `RUN` can read it, not only the one
  that mounts it (threat model, section 6). ranikust reads it once at the
  start (ADR-0078) but cannot hide it.
- **Options:** (a) warn for each one and document it; (b) refuse a file
  secret there, and accept secrets only from ranikust's environment; (c)
  read it at the start, then require the operator's tooling to remove it
  before the first `RUN` (unenforceable).
- **Recommendation:** (b), if security comes before kaniko's
  `--secret`-file habit: a secret the maintainer hands to one `RUN`
  should not reach every `RUN`. (a) keeps such builds working.
- **Interim:** (a): a warning per file secret in kaniko mode on `/`.
- **Raised:** 2026-11-02

### Q-446: Should kaniko mode on `/` read the resolver files once?

- **Answer (2026-11-03):** "Q-446: theo đề xuất": option (a), the
  recommendation: kaniko mode on `/` keeps reading `/etc/hosts` and
  `/etc/resolv.conf` at every lookup, as ADR-0051 accepts. Option (b) is
  taken up only if a measurement shows a case that (a) does not cover (the
  recommendation noted that (c) breaks registries behind DNS round-robin or
  short TTLs). No new ADR: ADR-0051 and ADR-0078 stand, and the threat model
  (section 6) and the kaniko-mode design say it.
- **Blocks:** nothing yet; **Area:** `rnk-registry`, `ranikust`
- **Context:** the C library reads `/etc/hosts` and `/etc/resolv.conf` at
  every lookup. In kaniko mode on `/` a `RUN` can edit both and redirect
  ranikust's later requests. ADR-0051 sends credentials only over verified
  TLS there, so a redirect can only fail a request, or answer an
  anonymous pull of a tag (digests are still checked). ADR-0078 reads
  ranikust's own configuration once; these files are the system's, read
  per lookup.
- **Options:** (a) keep it, as ADR-0051 accepts; (b) give `reqwest` a
  resolver that reads both files once, at the start (hickory or a
  custom `Resolve`); (c) resolve every registry the build names at the
  start and pin the addresses.
- **Recommendation:** (a) until a measurement shows a case that (a) does
  not cover; (b) if one does. (c) breaks registries behind DNS
  round-robin or short TTLs.
- **Interim:** (a).
- **Raised:** 2026-11-02

### Q-451: Should a `RUN`'s init be a small executed binary instead of a copy of ranikust?

- **Answer (2026-11-03):** "Q-451: (a) với mức chi phí hiện tại": option (a):
  the init of a full-sandbox `RUN` stays a copy-on-write copy of ranikust,
  at the cost measured above. Option (b), an executed small init, is not
  built; it was the same mechanism as Q-422's option (c), which
  [ADR-0082](adr/0082-full-sandbox-binds-the-binary-read-only-over-itself.md)
  does not take. Answered here only: ADR-0080 is accepted, so its body is
  not edited beyond its marker, and ADR-0082 notes the answer.
- **Blocks:** nothing yet; **Area:** `rnk-sandbox`, memory
- **Context:** the init of every full-sandbox `RUN`
  ([ADR-0080](adr/0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md))
  is a copy-on-write copy of ranikust that lives as long as the `RUN`: it
  keeps the pages ranikust had when it was cloned, so memory ranikust frees
  or rewrites meanwhile stays charged to the build's cgroup once per live
  init. Measured in the VM (kernel 6.8, overlay mode, 6 parallel stages,
  each writing 24 MiB then sleeping 20 to 70 s, in a cgroup of its own):
  the cgroup's anonymous memory peaked at 49.1 MiB against 45.7 MiB on main
  (+3.3 MiB), and while inits lived it stayed 9 to 12 MiB above main's (with
  5 inits at 24 s: 48.1 against 36.0 MiB); `memory.current` peaked at
  207.8 against 204.0 MiB **[measured]**. The worst case is one copy of
  ranikust's resident memory per live init, if ranikust rewrote all of it
  **[inferred]**; ranikust was 40 to 53 MiB resident here.
- **Options:** (a) keep the copy and its cost; (b) execute a small static
  init (embedded and run from a sealed memfd) that holds no copy of
  ranikust: the same mechanism as Q-422's option (c), so one design could
  answer both; (c) have the init release its private writable mappings
  (`madvise(MADV_DONTNEED)` over the ranges `/proc/self/maps` lists), which
  needs a parse in the child and care for the stack it runs on.
- **Recommendation:** (a) at these numbers; (b) together with Q-422 if
  either needs it.
- **Interim:** (a).
- **Raised:** 2026-11-03

### Q-452: How is a root `RUN` kept from delaying its init's death?

- **Answer (2026-11-03):** "Q-452: (a) khi có cgroup delegate, (b) khi tới
  Phase 3": option (a) where a delegated cgroup v2 tree exists (the command in
  a cgroup of its own, apart from the init's, ended with `cgroup.kill`), and
  option (b) in Phase 3 (`--userns` leaves the init's uid unmapped); option
  (c) is the state until each is built, not the choice. The implementation is
  a follow-up for each. Recorded in
  [ADR-0083](adr/0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md).
- **Blocks:** nothing yet; **Area:** `rnk-sandbox`, T8
- **Context:** when ranikust dies, the init of a full-sandbox `RUN`
  ([ADR-0080](adr/0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md))
  dies and the namespace with it, but a root `RUN` can delay that: as a
  process of the init's uid it may make the init `SCHED_IDLE` and pin it to
  one CPU without `CAP_SYS_NICE`, then keep that CPU busy. Measured in the
  VM (kernel 6.8): 0.9 to 1.8 ms without that, 7.6 s with 8 spinners,
  30.6 s with 32, about 1 s per spinner, bounded only by the outer cgroup's
  `pids.max` **[measured]** (threat-model section 6,
  `red_team_a_run_can_slow_its_inits_death_but_not_stop_it`). The `RUN`'s
  processes keep running meanwhile. Running the init under a uid of its own
  does not help: a root `RUN` has `CAP_SETUID` among Docker's default
  capabilities and switches to that uid, after which renicing and pinning
  the init were allowed again **[measured]** (VM, 2026-11-03).
- **Options:** (a) put the command in a cgroup of its own, apart from the
  init's: group scheduling then gives the init's cgroup its share of the CPU
  however many spinners the `RUN` starts **[inferred]**, and a write to the
  `RUN` cgroup's `cgroup.kill` ends its whole tree as a second line; this
  needs a writable, delegated cgroup v2 tree, which a default Docker
  container or most pods do not give (ADR-0007: cgroups are only read
  today); (b) with `--userns` (Phase 3), run the command in a user namespace
  that does not map the init's uid, so that neither the same-owner rule nor
  `CAP_SETUID` inside it reaches the init **[inferred]**; (c) keep the
  documented residual.
- **Recommendation:** (a) where a delegated cgroup exists; (b) with
  Phase 3.
- **Interim:** (c).
- **Raised:** 2026-11-03

### Q-453: Where does kaniko mode on `/` put a secret file, and which directory does the snapshot ignore?

- **Answer (2026-11-03):** "Q-453 chọn (a)+(d), gỡ cả hai luật": options (a)
  and (d). Only the secret file's own path goes on the snapshot ignore list
  (the `src=` file, the placed file at its target), never its directory; the
  file placed for a `RUN` is removed after that `RUN`; both refusals that
  ADR-0084 lifts go (the start-up check `paths_outside_the_root` and ADR-0057's
  refusal in `direct_mounts`). Options (b), (c) and (e) are not taken. The
  implementation is a follow-up. Recorded in
  [ADR-0085](adr/0085-a-secret-file-on-slash-goes-at-its-target-is-ignored-by-its-own-path-and-removed-after-its-run.md).
- **Blocks:** the implementation of
  [ADR-0084](adr/0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md);
  **Area:** `ranikust`, `rnk-builder`, `rnk-sandbox`, `rnk-snapshot`
- **Context:** ADR-0084 lets a secret be a file in kaniko mode on `/` and puts
  "the directory that holds the secret files" on the snapshot ignore list
  (the maintainer, 2026-11-03: "Bỏ qua thư mục này khi build"). His words do
  not say which directory. Two kinds of file are involved:
  - a `--secret src=` file, wherever the operator put it, read once at the
    start (ADR-0078); today it must be on a volume or an `--ignore-path`, or
    the build is refused (`paths_outside_the_root`);
  - the file a `RUN --mount=type=secret` reads. On `/` there is no tmpfs for
    the `RUN` (ADR-0057), so ranikust would write it into the `/` all `RUN`s
    share before the `RUN`. BuildKit's place is the mount's `target=`, else
    `/run/secrets/<id>`, mode `0400` (`secret_mount` in `mounts.rs`).
  An ignored entry covers everything below it, which the walk skips, base
  extraction does not replace and emptying the root does not delete
  (`IgnoreList`; a path is physical, so `/var/run` ignores the symlink and not
  `/run`). Ignoring the directory of `--secret src=/etc/token` would be
  ignoring `/etc`.
- **Options:** which entry goes on the list: (a) the secret file's own path
  only (the `src=` file, the placed file at its target); (b) the file's
  directory, whatever it is; (c) a directory ranikust fixes for placed files,
  `/run/secrets`, with a `target=` outside it refused, and the `src=` file's
  own path as in (a). Independently, for the placed file: (d) removed after its
  `RUN` ends, so a later `RUN` cannot read it and a crash leaves it only until
  the container ends; (e) left until the build ends.
- **Recommendation:** (a) with (d): the narrowest ignore, so no sibling file
  of the image drops out of a layer, and the placed file readable by other
  `RUN`s only while its own `RUN` lasts; (b) can hide a system directory.
- **Interim:** both refusals stand as they are (`paths_outside_the_root`,
  `direct_mounts`), the `--secret src=` warning stays; nothing is built.
- **Raised:** 2026-11-03

### Q-438: Does ranikust refuse a context file its owner could not read?

- **Answer (2026-11-03):** option (a), settled in the host-safety fix loop
  of ADR-0076: a context file the owner of the directory it is in could not
  read by its mode bits is refused (owner bits for the directory's owner,
  group bits for the directory's group, else other bits; root's
  directories read anything unless their group or others may write them).
  It is an approximation: supplementary groups are not looked up and ACLs
  are not read, so it fails closed in the usual direction only (a file
  readable through another group or an ACL grant is refused) and can pass
  a file an ACL deny entry or a group-excluding mode would deny. Recorded
  in [ADR-0076](adr/0076-the-build-context-owner-is-a-lower-trust-principal.md);
  (b) can follow with Phase 5's context reader.
- **Area:** `rnk-context`, `rnk-rootfs` (security)
- **Context:** ranikust reads context files as root, so `COPY` would copy
  a file the context's owner cannot read. ADR-0076 refuses the case an
  unprivileged owner can set up: a hardlink to a file from outside, with
  another owner, where `fs.protected_hardlinks=0`. A single-link file of
  another owner in the context (one root put there) is still read.
  BuildKit reads the context as the invoking user, so such a file is not
  readable there.
- **Options:**
  - (a) Refuse a file the owner's uid, gid and supplementary groups could
    not read by its mode bits. Exact only without ACLs, and the groups come
    from the host's NSS.
  - (b) Read context files with the owner's filesystem credentials
    (`setfsuid`/`setfsgid` and supplementary groups on a dedicated thread),
    as BuildKit's client reads them.
  - (c) Keep the hardlink rule only.
- **Recommendation:** (b) when the context reader gets its own thread
  (Phase 5's remote contexts); until then (c).
- **Interim:** (c), ADR-0076's hardlink rule.
- **Raised:** 2026-11-02

### Q-033: Which performance conditions are still unmeasured?

- **Answer (2026-11-01):** "5. theo ý bạn": closed with what Phases 1
  and 2 measured; what is left moves to Phase 6 as
  [Q-421](https://github.com/tran4774/ranikust/issues/29).
  Measured:
  - walk and hash against kaniko: kaniko's `snapshot.walk` 3.18 s and
    `resolve` 2.48 s on W1
    ([experiment 07](research/experiments/07-kaniko-vs-overlay.md));
    ranikust's ten W1 walks 3.3 s in the VM (ADR-0061's run);
  - parallel stages: W3 −41 to −44 % at 32 CPUs, −21 % at `--cpus=4`
    ([experiment 12](research/experiments/12-kaniko-mode-parallel.md));
    the `par-stages` bench (four independent stages against the same work
    chained, VM, 4-CPU cap and uncapped): per-stage kaniko mode −43 to
    −45 %, overlay mode −38 to −40 %, peak memory about 550–650 MiB
    ([testing](testing.md), criterion 7);
  - the shortened VM e2e runs of 2026-11-01 (one round each, builders
    capped at 4 CPUs and 8 GiB, quiet host): W1 kaniko 23.4–26.2 s,
    overlay 20.5–21.2 s, kaniko mode on a container's `/` 21.8–22.9 s,
    per-stage kaniko mode 29.5–31.6 s before and 22.1 s after ADR-0061's
    hand-over (Q-410); W3 kaniko 28.9–30.3 s, overlay 14.1–16.0 s, kaniko
    mode on `/` 20.5–21.5 s, per-stage 14.5–17.2 s; every build's peak
    memory now recorded per builder;
  - `--single-snapshot`: 0.61 s saved at 80 000 files and 1.16 s at
    250 000 files over 10 steps (Q-420, ADR-0065);
  - the e2e harness itself: flatten 23.4 s → 3.9 s (1.1 s from its cache)
    on a 220 000-path image; the light W0 suite in about 25 s, VM boot
    included.
- **Context (as asked):** not measured then: `sha2` against
  `ring::digest`, real network registries, export on memory-starved
  runners, cold page cache, slow disks and deletion of very large roots,
  `--snapshot-mode=redo` and `time` against `full`. All carried into
  Q-421.

### Q-360: What does kaniko mode on `/` do after an earlier build in the container was cut short?

- **Answer (2026-11-01):** "4: ok chốt a": option (a), the interim
  behaviour stays. A store opened on this container's marker or on an
  earlier store's leftover names makes no name for the build (state in
  `memfd`, warned with why and how to avoid it: ranikust as PID 1, or
  restart the container), keeps the marker, and a `COPY --from` a kept
  stage or an image unpacked by name fails with that advice. Recorded in
  [ADR-0066](adr/0066-kaniko-mode-on-root-makes-no-names-after-a-build-cut-short.md).
- **Context (as asked):** found by the host-safety review of ADR-0056: the
  fence holds only this build's `RUN` processes, so a `RUN` of an earlier
  ranikust killed mid-`RUN` (not PID 1) may live on and open a name in the
  moment it exists. Options were (a) no names, `memfd`; (b) refuse; (c)
  kill non-ancestors (unsafe); (d) names only as PID 1.

### Q-420: Does kaniko mode offer `--single-snapshot`?

- **Answer (2026-11-01):** "1. theo đề xuất": option (b), refused for good
  in every mode, whatever its value, from the table of refused kaniko flags
  (exit 2, before the arguments are parsed). Recorded in
  [ADR-0065](adr/0065-single-snapshot-refused-for-good.md).
- **Context (as asked):** measured with `rnk-snapshot`
  `single_snapshot_poc` (release build, ext4, page cache warm, 10 steps of
  200 rewrites and 200 new files): one walk at the end saves 0.61 s at
  80 000 files and 1.16 s at 250 000 files (about W1's size), up to about
  3 s on W1 [inferred], against the per-step layers Phase 4's cache reuses
  on a rebuild.

### Q-410: How do per-stage directories stop costing more than kaniko on W1?

- **Answer (2026-11-01):** "1. ok" (to the recommendation, option (c));
  then "ok làm Q410c trước". The last stage to read a parent takes its
  directory over instead of copying it (pruned of what the copy leaves
  out, then moved with `renameat2(RENAME_NOREPLACE)`), and a stage
  directory nobody reads any more is released at once on a release
  thread, the build's end waiting for it. Recorded in
  [ADR-0061](adr/0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md);
  which reader takes the directory is decided from the stage graph (its
  only reader), never by scheduling:
  [ADR-0062](adr/0062-stage-directory-hand-over-decided-from-the-stage-graph.md).
- **Context (as asked):** the first VM `e2e` run with kaniko mode took
  31.3 s on W1 as root on the VM with a directory per stage, against
  kaniko's 23.7 s: `FROM <stage>` copies 7.2 s (ext4, no reflink), the
  release at the end 4.2 s.

### Q-400: How does `--parallel-stages` run stages at once on a container's `/` without mounts?

- **Answer (2026-11-01):** "Ok làm theo đề xuất của bạn": option (a).
  `--parallel-stages` stays refused with exit 2 and guidance, whatever
  the environment: outside a container stages already run in parallel
  with the full sandbox (ADR-0059), and on a container's `/` a bare
  `chroot` per stage lets a `RUN` escape to what ADR-0051 and ADR-0056
  keep out of its reach. Option (b), a hardened bare `chroot`
  (capabilities dropped after the `chroot`, static fake `/proc`, no
  procfs), may be reconsidered only after `poc-runner` measures its escape
  vectors in the VM (rootful Docker, Kubernetes' default profile,
  `ptrace_scope` 0 and 1); option (c), ADR-0014 as written, never.
  Recorded in
  [ADR-0060](adr/0060-copy-leaves-special-files-out-where-run-has-no-nodev.md).

### Q-380: How does a `Direct` `RUN` on `/` get a secret as a file?

- **Answer (2026-11-01):** "Q380-382 làm theo đề xuất": option (a). A
  secret as a file (no `env=`, or a `target=`) is refused in kaniko mode
  on `/`, pointing to `env=` or overlay mode; a secret never touches the
  image's filesystem. Options (b) a file on `/` removed before the walk,
  (c) a symlink to ranikust's memfd and (d) an inherited memfd with its
  `/dev/fd` path were not taken. Recorded in [ADR-0057](adr/0057-direct-run-heredoc-from-memfd-and-refusals.md).

### Q-381: How does a `Direct` `RUN` on `/` get a cache mount?

- **Answer (2026-11-01):** "Q380-382 làm theo đề xuất": option (c). A
  cache mount of a `RUN` in kaniko mode on `/` is left out with a warning
  and the `RUN` runs without the cache; the warning says that a
  `sharing=locked` cache then serializes nothing. A cache with `from=`
  stays refused (Phase 5). Option (b), copying the cache in and out, was
  not taken. Recorded in [ADR-0057](adr/0057-direct-run-heredoc-from-memfd-and-refusals.md).

### Q-382: What does `--network=none` do for a `Direct` `RUN` on `/`?

- **Answer (2026-11-01):** "Q380-382 làm theo đề xuất": option (a). The
  `--network=none` flag is refused at start-up, before the pull, and a
  `RUN --network=none` at that `RUN`; a build that asked for no network
  never gets the container's. Options (b) a seccomp filter and (c) a user
  namespace remain possible later; (d) is never taken. Recorded in [ADR-0057](adr/0057-direct-run-heredoc-from-memfd-and-refusals.md).

### Q-385: Does a step that changes nothing still add an empty layer?

- **Answer (2026-11-01):** "Q385 làm như kaniko": option (b). A step
  whose layer is empty adds no layer, only its history entry with
  `empty_layer: true`, in both modes; this changes Q-131's answer for
  such steps. Raised from the issue mining (moby/buildkit#6543: repeated
  empty layers refused by containerd). Recorded in
  [ADR-0058](adr/0058-a-step-that-changes-nothing-adds-no-layer.md).

### Q-011: Who owns the workdir checks, and what counts as empty?

- **Answer (2026-10-30):** options (b) and (d): `ranikust` runs the workdir
  checks during process set-up and holds the kaniko-mode workdir `flock` for
  the build; `rnk-rootfs` provides the primitives only. A workdir of only
  mount points is refused. Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-019: What is the final name of the parallel-stages flag?

- **Answer (2026-10-30):** option (a): `--parallel-stages` is the final
  name. Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-034: When must a lazily skipped stage still be snapshotted?

- **Answer (2026-10-30):** the maintainer took the recommendation: decided
  once from the stage graph before the build. A stage's layers are needed
  when it is the target or a stage whose layers are needed builds `FROM` it;
  any other stage is not walked. Nothing is decided later, so no deletion
  has to be found after the fact; `--cache` (Phase 4) only adds inputs to
  the rule (Q-042). Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-035: What does the kaniko-mode ignore list hold?

- **Answer (2026-10-30):** options (a) and (c): all of `/ranikust`, and
  kaniko's `/etc/mtab`, `/tmp/apt-key-gpghome` and `/var/run` (while
  `--ignore-var-run` is true), besides mount points, `--ignore-path`,
  `/proc`, `/sys`, `/dev` and the state directory. Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-036: What is the state directory default for kaniko mode on a host?

- **Answer (2026-10-30):** a third option from the maintainer: with the root
  `/` the default stays `/ranikust/state`; with any other root it is
  `<cwd>/.ranikust`, a hidden directory in the directory ranikust was
  started in, ignored whenever it lies inside the root. Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-037: What else must the fake proc cover?

- **Answer (2026-10-30):** option (b): keep the fourteen files until a build
  fails without `memory.stat`; the untried tools get tests in Phase 2 task
  10. Recorded in [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-038: Which crates fill the Phase 2 dependency picks?

- **Answer (2026-10-30):** options (b) and (d), after the benchmark of
  Phase 2 task 0 (`tests/bench/walk-hash`): the walk is a `rayon` scope over
  `openat2` and `getdents64`, the content hash xxh3-128 from `xxhash-rust`.
  The walk runs on 8 workers by default; the maintainer asked for a flag to
  change it, `--snapshot-workers`. Recorded in
  [ADR-0039](adr/0039-kaniko-mode-walk-and-hash-crates.md).

### Q-270: Are device nodes from an image created in a host-workdir root?

- **Answer (2026-10-31):** option (b), by the rule to follow kaniko:
  `extract_live` creates no block or character device node and no FIFO in a
  live root, `/` and host workdirs alike, as kaniko's `ExtractFile`
  (kaniko@44aa7ddc0 `pkg/util/fs_util.go:440-575`); they are counted as
  `nodes_skipped`, and the image keeps them through its unchanged base
  layers. Recorded in
  [ADR-0041](adr/0041-live-root-extraction-skips-device-nodes.md).

### Q-300: Is a stage whose image is kept for a later `FROM` walked in sequential kaniko mode?

- **Answer (2026-10-31):** option (a), kaniko's `SaveStage` behaviour:
  every unit with `save_image` is walked. Recorded in
  [ADR-0044](adr/0044-kaniko-mode-walks-every-kept-stage.md).

### Q-024: What is the Snapshotter signature?

- **Answer (2026-10-31):** the interim shape, with associated types:
  `Snapshotter::Layer` (a base layer as the mode keeps it) and
  `StageRootfs::Diff: ExportLayer` (a closed layer), and
  `StageRootfs::set_layers_needed` for lazy snapshots, so overlay and kaniko
  mode share one builder. Recorded in
  [ADR-0046](adr/0046-snapshotter-signature.md).

### Q-320: What keeps a kaniko-mode host workdir out of other users' reach?

- **Answer (2026-10-31):** option (a), chosen by the maintainer: a real
  parent of the workdir that group and others cannot search, and it and
  everything below it down to the workdir owned by root and not writable
  by others; directories above it may belong to anyone; a missing
  `nosuid,nodev` is warned about. Recorded in
  [ADR-0047](adr/0047-kaniko-workdir-chain-is-private.md).

### Q-321: Is the container check enough to guard a kaniko-mode build on `/`?

- **Answer (2026-10-31):** option (a), by the maintainer: kaniko's
  container heuristic and `--force`, so a container build needs no extra
  flag; revisit with (b) if a host is seen to match the heuristic.
  Recorded in [ADR-0048](adr/0048-container-heuristic-guards-the-root.md).

### Q-008: What does a repeated target flag mean?

- **Answer (2026-10-31):** option (b): one `--target`; a repeated flag or a
  comma-separated list is a usage error (exit 2). The maintainer, with the priority of
  security, then performance; recorded in
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md).

### Q-010: Which path type does rnk-ignore match on?

- **Answer (2026-10-31):** option (b), bytes, as built: a file whose name is not
  UTF-8 still matches an exclude. The maintainer, with the priority of
  security, then performance; recorded in
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md).

### Q-047: What are the mirror, registry-map and helper flags called?

- **Answer (2026-10-31):** option (c): a mirror or mapped registry never
  receives another registry's credentials and never serves a push; the
  flags are named when Phase 5 builds them, kaniko's names only where the
  meaning is exactly that. The maintainer, with the priority of
  security, then performance; recorded in
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md).

### Q-260: Should overlay mode honour kaniko's snapshot flags?

- **Answer (2026-10-31):** option (d): overlay mode refuses `--ignore-path`,
  `--single-snapshot` and `--ignore-var-run=false` for good; kaniko mode
  keeps the first two while a case needs them and `--single-snapshot` only
  if measured faster. The maintainer, with the priority of
  security, then performance; recorded in
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md).

### Q-048: What does the push subcommand take, and when does it come?

- **Answer (2026-10-31):** an OCI layout only, what `--oci-layout-path`
  writes; no docker-save tar; no phase yet. `login` is dropped (ADR-0049).
  Recorded in [ADR-0052](adr/0052-push-input-aliases-and-issue-mining-fixes.md).

### Q-110: What is the overlay-mode state directory without `--workdir`?

- **Answer (2026-10-31):** a fixed default, `/var/lib/ranikust` as root in
  every mode, `--state-dir` to change it; `--workdir` is gone: ranikust
  picks the build root itself. Recorded in
  [ADR-0050](adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md).

### Q-162: Where do the blobs of built layers live?

- **Answer (2026-10-31):** option (b), in Phase 4: published in the
  persistent store under the digest computed while writing, for overlay
  mode and kaniko mode on a host; kaniko mode on `/` keeps no state
  between runs (ADR-0051). Recorded in
  [ADR-0052](adr/0052-push-input-aliases-and-issue-mining-fixes.md).

### Q-330: What does kaniko mode do on a host where root may not mount?

- **Answer (2026-10-31):** option (a), by the maintainer: refused, with the
  advice to run ranikust in a container or give it mount privileges; `RUN`
  would otherwise run in a bare `chroot` on the host (the options were (a)
  refuse, (b) an opt-in flag, (c) a warning). Recorded in
  [ADR-0050](adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md).

### Q-340: Where do kaniko mode's anonymous files live when the container's root has no O_TMPFILE?

- **Answer (2026-11-01):** the maintainer asked for ranikust's memory to be
  as small as possible, safely ("Làm 1 và 2 đi. Làm sao tối ưu bộ nhớ nhất
  và an toàn là được"): on disk. Where `O_TMPFILE` is refused, a file made
  by a random name and unlinked at once, only while no `RUN` process
  exists (a fence in the store); `memfd` is the last fallback, with a
  warning. Streaming layers (option 1) is ADR-0055. Recorded in
  [ADR-0056](adr/0056-kaniko-mode-state-on-disk-behind-a-run-fence.md).

### Q-350: How does kaniko mode on `/` keep a stage's files out of `RUN`'s reach?

- **Answer (2026-11-01):** option (a), as ADR-0051 decided for all state on
  `/`: a kept stage is a plain tar in an anonymous file, an image read by
  `COPY --from` stays its blobs, and a reader's view is unpacked in the
  state directory only while no `RUN` runs and deleted at the end of the
  instruction that read it. Recorded in
  [ADR-0056](adr/0056-kaniko-mode-state-on-disk-behind-a-run-fence.md).

### Q-140: How is the aarch64 binary made static-pie with symbols?

- **Answer (2026-10-30):** the maintainer chose a fourth option: link with
  a gcc driver that targets aarch64, as x86_64 links with the host `cc`:
  the native `cc` on an aarch64 machine (CI's `ubuntu-24.04-arm` runner)
  and `aarch64-linux-gnu-gcc` (package `gcc-aarch64-linux-gnu`) on
  x86_64. The linker alone is not enough: rustc 1.99's
  `aarch64-unknown-linux-musl` target has no static-pie and links
  `-static -no-pie` with `crt1.o` (forcing `-static-pie` over it gives a
  binary that crashes, since `crt1.o` does not relocate itself). `just
  test-aarch64` therefore builds PIE code (`-C relocation-model=pie`),
  turns off rustc's own start files (`-C link-self-contained=no`) and links
  musl's self-relocating `rcrt1.o`, `crti.o`, `crtbeginS.o`, `libc.a`,
  `crtendS.o` and `crtn.o` from rustc's self-contained directory with
  `-static-pie -nostdlib`, as rustc does by itself for x86_64. Measured on
  x86_64 with `aarch64-linux-gnu-gcc` 15.2 and QEMU 10.2: `static-pie
  linked, not stripped`, no interpreter, no `NEEDED`, no glibc symbol;
  every T0 test passes. Revisit when rustc gives the target static-pie
  (the flags can then go). Without a cross gcc the recipe falls back to
  zig's linker and warns. See
  [development-setup](development-setup.md#2-aarch64-when-you-touch-anything-platform-specific).

### Q-020: Is custom-platform accepted as an alias of platform?

- **Answer (2026-10-30):** the maintainer chose option (a):
  `--custom-platform` is an alias of `--platform`, and the deprecated
  `--customPlatform` too, with kaniko's warning. Recorded in
  [ADR-0036](adr/0036-kaniko-flags-in-phase-1.md).

### Q-021: What do the cache flags do before Phase 4?

- **Answer (2026-10-30):** option (a) with one exception: `--cache`,
  `--cache=true` and `--cache-repo` are refused naming Phase 4, and
  `--cache=false`, which asks for what Phase 1 does, is accepted. Recorded
  in [ADR-0036](adr/0036-kaniko-flags-in-phase-1.md).

### Q-023: Which other kaniko flags and variables are accepted?

- **Answer (2026-10-30):** the maintainer approved the per-flag list:
  accepted with kaniko's meaning, accepted and ignored with a warning where
  the image is the same without the flag (extending `ADR-0023`), refused
  until a named phase, or refused for good; `KANIKO_NO_PUSH` overrides the
  flag, `KANIKO_REGISTRY_MIRROR`/`_MAP` exit 2, `DOCKER_AUTH_CONFIG` is read
  before `config.json`, every other set `KANIKO_*`/`FF_KANIKO_*` variable
  gets one warning, and there is no `KANIKO_METRICS_FILE` alias. Recorded in
  [ADR-0036](adr/0036-kaniko-flags-in-phase-1.md); the table is in
  [kaniko-map](agents/kaniko-map.md#flags).

### Q-031: How does per-stage log grouping fit the kaniko log flags?

- **Answer (2026-10-30):** option (a): `--verbosity`, `--log-format` and
  `--log-timestamp` take kaniko's meaning, and per-stage grouping stays a
  separate matter, independent of the format. Recorded in
  [ADR-0036](adr/0036-kaniko-flags-in-phase-1.md).

### Q-049: Which overlay optimizations come after Phase 1?

- **Answer (2026-10-30):** the maintainer chose option (a): the exporter
  leaves out every unchanged copy-up by BuildKit's full `sameDirent` rule,
  contents compared when both times are whole seconds, and writes an
  opaque directory as whiteouts of what it hides. Recorded in
  [ADR-0034](adr/0034-unchanged-copy-ups-left-out-of-layers.md). Keeping
  one mount for consecutive instructions moved to
  [Q-245](https://github.com/tran4774/ranikust/issues/18).

### Q-001: Who owns the persistent store and `BuildPaths`?

- **Answer (2026-10-30):** a new crate, `rnk-store`, owns `blobs/`,
  `layers/`, `caches/`, `tmp/`, the per-digest locks, start-up cleanup,
  `prune` and `BuildPaths`. Recorded in
  [ADR-0021](adr/0021-rnk-store-crate.md).

### Q-002: What is the mode-selection flag, given kaniko's `--snapshot-mode`?

- **Answer (2026-10-30):** `--mode=auto|overlay|kaniko`;
  `--snapshot-mode=full|redo|time` keeps kaniko's meaning. Recorded in
  [ADR-0023](adr/0023-mode-flag.md).

### Q-003: Where does the store live in each mode?

- **Answer (2026-10-30):** one `--state-dir` for every mode, by default
  `<workdir>/.ranikust` in overlay mode and `/ranikust/state` in kaniko mode.
  Recorded in [ADR-0022](adr/0022-common-state-dir.md).

### Q-004: Which TLS stack does the registry client use?

- **Answer (2026-10-30):** rustls with the `ring` provider, trusting the
  host's CAs plus the embedded webpki roots. Recorded in
  [ADR-0024](adr/0024-tls-rustls-ring.md).

### Q-005: Which branch do feature branches start from?

- **Answer (2026-10-30):** the latest `main`. The repository set-up was
  merged into `main`; every feature, fix or refactor starts from it on its
  own branch and is committed at each checkpoint. Recorded in
  [rules.md](agents/rules.md#branches-and-commits).

### Q-050: Which result does the context walk follow when BuildKit's two matchers disagree?

- **Answer (2026-10-30):** follow BuildKit's walk
  (`matches_using_parent` plus `may_reinclude_below`). Recorded in
  [ADR-0027](adr/0027-context-filtering-follows-buildkit-walk.md), which
  supersedes the `.dockerignore` bullet of ADR-0015.

### Q-051: Does `FROM <stage>` match stage names case-insensitively?

- **Answer (2026-10-30):** follow BuildKit: `FROM <stage>` matches stage
  names exactly. kaniko's `Dockerfile_test_issue_519` is an accepted
  difference ([kaniko-map](agents/kaniko-map.md#behaviour-to-match)).

### Q-053: Which entry order and which names does the layer tar use?

- **Answer (2026-10-30):** follow the OCI layer specification's
  recommendation: depth first, the opaque marker and whiteouts before their
  siblings; files really named `.wh.*` are rejected. Recorded on the
  [rnk-layer card](agents/crates/rnk-layer.md) and in
  [overlay-snapshot](design/overlay-snapshot.md).

### Q-056: How are tar extension headers bounded?

- **Answer (2026-10-30):** no bound for now; GNU sparse entries stay
  refused. Revisit with `cargo fuzz` on `extract` in Phase 6
  ([rootfs](design/rootfs.md)).
- **Revised (2026-11-02):** bounded at 1 MiB by default, the `tar-header`
  limit of `--limit` ([ADR-0079](adr/0079-resource-limits-with-a-limit-flag.md)),
  checked before the `tar` crate reads a body. Fuzzing `extract` stays a
  Phase 6 task.

### Q-052: How does zstd reach the static musl build?

- **Answer (2026-10-30):** cargo-zigbuild with zig in the `justfile` and
  CI, as [ADR-0020](adr/0020-static-musl-via-cargo-zigbuild.md) planned;
  the same toolchain builds mimalloc. zstd is added after it
  ([phase-1](agents/phase-1.md#task-order), task 5a).
- **Implemented (2026-10-30):** `just build-static` and `just
  check-aarch64` run cargo-zigbuild 0.24.0 with zig 0.16.0, the same in
  CI; zig compiles every C dependency (libzstd, mimalloc, ring) and the
  host `cc` links x86_64 to keep static-pie. `rnk-layer` writes zstd as
  independent 1 MiB frames.

### Q-054: What is the minimum kernel, and who checks it?

- **Answer (2026-10-30):** require Linux 5.8; the binary probes `openat2`
  and the `statx` mount id at start-up and refuses with guidance. Recorded
  in [ADR-0028](adr/0028-minimum-kernel-linux-5-8.md).

### Q-055: How does overlay-form extraction handle a hardlink into a lower layer?

- **Answer (2026-10-30):** option (b): copy the target's content from the
  lower layers' extracted directories as a regular file, which splits the
  hardlink as overlay copy-up does (`ADR-0012`). Recorded in
  [rootfs](design/rootfs.md#extraction); implemented with base-layer
  extraction (Phase 1 task 9).
- **Implemented (2026-10-30):** `rnk_rootfs::extract_overlay(dst, tar,
  lowers)` looks the target up in the lower layers as overlay's merged
  view (whiteouts, opaque directories) and copies it with its metadata
  and xattrs; `rnk-snapshot` still has to call it.

### Q-012: Which crates fill the Phase 1 dependency picks?

- **Answer (2026-10-30):** the picks the code already made: `rustix` for
  syscalls (with `libc` where `rustix` lacks a call), not `nix`; `sha2` for
  sha256, not `aws-lc-rs` (a C and CMake build, kept out by `ADR-0024`);
  gzip by hand on `flate2`/zlib-rs, not `gzp` (not deterministic across
  thread counts, `exp-05`); `reqwest`, the stack `oci-client` builds on,
  not `hyper` directly; no `docker_credential`: `rnk-registry` reads
  `config.json` itself and credential helpers come in Phase 5 (Q-047);
  `clap` (derive) for the CLI. None of them appears in a library's public
  API. Whether `ring::digest` beats `sha2` on large layers is measured
  under Q-033. Recorded in
  [ADR-0030](adr/0030-phase-1-dependency-picks.md).

### Q-075: Which host name does a RUN see?

- **Answer (2026-10-30):** a fixed `ranikust` (option c), whatever the
  environment, so a `RUN` that records its host name gives the same bytes
  on every host; no flag. `rnk-builder` may still set
  `RunSpec::hostname`. Implemented in `rnk-sandbox` (`DEFAULT_HOSTNAME`).
  Recorded in [ADR-0031](adr/0031-run-host-name-and-hosts-file.md).

### Q-076: What /etc/hosts does a RUN see?

- **Answer (2026-10-30):** option (c): with the host network, ranikust's
  own `/etc/hosts` (as kaniko's `RUN` sees it, so Kubernetes `hostAliases`
  keep working) plus the host name; with `--network=none`, loopback names
  plus the host name, as BuildKit. Implemented in `rnk-sandbox`
  (`plan.rs`, `hosts_file`). Recorded in
  [ADR-0031](adr/0031-run-host-name-and-hosts-file.md).

### Q-006: Which image format does ranikust write by default?

- **Answer (2026-10-30):** OCI media types by default, whatever the base
  image's format and `--reproducible`, with gzip layers; `--image-format`
  (Q-023) is the override. BuildKit's exporter defaults to OCI and converts
  base layers' media types. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-007: Which image-assembly types live in rnk-oci?

- **Answer (2026-10-30):** option (a): `ConfigEdit`,
  `ImageConfig::push_layer`/`push_history`, `DockerTarImage` and `OciLayout`
  stay pure types in `rnk-oci`; `rnk-builder` writes the files. An internal
  split with no BuildKit counterpart, so the interim is adopted.

### Q-009: Does the parser accept instructions deferred to Phase 5?

- **Answer (2026-10-30):** option (a): the parser accepts every instruction
  and flag BuildKit parses; the builder refuses what it cannot run yet,
  naming the phase. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-013: How does the layer writer learn missing parent metadata?

- **Answer (2026-10-30):** option (a): the writer takes each missing
  parent's metadata from the merged view, as containerd's `includeParents`
  does for BuildKit. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-014: Are cross-repository mounts part of Phase 1?

- **Answer (2026-10-30):** yes: push mounts blobs across repositories and
  falls back to an upload when the mount is refused, as containerd's pusher.
  Implemented in `rnk_builder::push`. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-022: What format does the secret flag take?

- **Answer (2026-10-30):** the full buildctl/kaniko syntax: `id=`,
  `type=file|env`, `src=`/`source=`, `env=`; with no source, the environment
  variable named by the id if set, else the file named by the id; a secret
  is at most 500 KiB. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-025: Are other user xattrs kept in exported layers?

- **Answer (2026-10-30):** no: exported layers carry only
  `security.capability`, as BuildKit's (containerd's) layer writer. The
  entry's claim that BuildKit strips only other `user.*` xattrs was wrong.
  Recorded in [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-026: When and how are layers merged near the 500-lower limit?

- **Answer (2026-10-30):** no merging: a stage that needs more lowers than
  the kernel allows fails with a clear error, as BuildKit's overlay
  snapshotter. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-043: What are the prune defaults?

- **Answer (2026-10-30):** age by last use (option a), with BuildKit's tiers
  as defaults: cache mounts and contexts dropped after 48 h unused beyond
  512 MB, anything unused for 60 days, and size caps of 10% reserved, 20%
  minimum free, 80% maximum used. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-044: How do remote ONBUILD triggers reach the stage graph?

- **Answer (2026-10-30):** option (b): a separate step extends the graph
  once base configs are fetched, repeated until no new dependency appears,
  as BuildKit's `initOnBuildTriggers`. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-046: What happens to HTTPS contexts on Azure Blob hosts?

- **Answer (2026-10-30):** option (b): fetch them as plain HTTPS, as
  BuildKit does for any `https://` context (an archive is unpacked, anything
  else is the Dockerfile). Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-065: Do localhost and private-network registries fall back to HTTP?

- **Answer (2026-10-30):** loopback registries (`localhost`, `127.0.0.0/8`,
  `::1`) use plain HTTP without a flag, as BuildKit; other hosts, RFC 1918
  and `*.local` included, fall back to HTTP only when listed with
  `--insecure-registry`. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-077: How is the MS_MOVE fallback kept from a chroot escape?

- **Answer (2026-10-30):** option (b), the recommendation, confirmed by the
  VM escapes: `MS_MOVE` only where `/` is a mount root, keeping Docker's 14
  capabilities; ranikust inside a plain chroot refuses. Measured with `just
  test-vm smoke` on kernels 5.15 and 6.8
  ([testing](testing.md#the-vm-harness-tier-t2)).

### Q-080: Who resolves COPY sources against the context, and how?

- **Answer (2026-10-30):** option (a) with BuildKit's rules: `rnk-builder`
  resolves sources against the context and stage roots; a wildcard that
  matches nothing copies nothing and is not an error; there is no "several
  sources need a trailing `/`" rule. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-081: Does the context sub-path flag apply to a local directory?

- **Answer (2026-10-30):** option (a): the sub-path applies only to fetched
  (git, HTTPS, tar) contexts and is ignored for a local directory, as both
  BuildKit and kaniko do. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).
- **Answer (2026-10-31):** "Q81 theo đề xuất": drop kaniko's `dir://`
  context syntax (option (b) of the recommendation). A local context is a
  plain path, and `--context-sub-path` applies only to fetched contexts
  (git, HTTPS, tar), as the answer above says. The code still joined the
  sub-path onto a `dir://` context; that changes with the Phase 2 task 7
  integration branch. Recorded in
  [ADR-0053](adr/0053-local-context-is-a-plain-path-no-dir-scheme.md).
- **Answer (2026-10-31):** "Q81 A": the two points ADR-0053 left open.
  (1) `--context=dir://...` is refused with a message to use a plain path;
  (2) `--context-sub-path` with a local context is refused (exit 2) with a
  message to pass the sub-directory itself as `--context`, since a flag
  with no effect is refused, not ignored (ADR-0049). The code change lands
  with the Phase 2 task 7 integration branch. Recorded in
  [ADR-0054](adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md), which completes ADR-0053.

### Q-100: How durable is a published store entry after a power loss?

- **Answer (2026-10-30):** option (a): blobs are fsynced before publication;
  extracted layers are not synced, as containerd's defaults under BuildKit.
  Recorded in [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-111: Does the store open before or after the probes?

- **Answer (2026-10-30):** option (a), the recommendation: `set_up` opens
  the store (marker check, start-up clean-up) right after the state
  directory check and before both probes (`feat/rnk-snapshot`).

### Q-120: Where kaniko and BuildKit copy differently, which does `copy` follow?

- **Answer (2026-10-30):** BuildKit's copy, item by item: (1) a symlinked
  source is followed; (2) all xattrs are copied (layers still keep only
  `security.capability`, Q-025); (3) an existing top-level destination keeps
  its metadata, existing directories below it take the source's; (4) devices
  and FIFOs are recreated and sockets become empty regular files; (5)
  `--chmod` applies to directories too; (6) symbolic `--chmod` (`u+x`) is
  accepted. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-121: Which overlay forms does the lower-layer lookup recognise?

- **Answer (2026-10-30):** option (a), the recommendation: only ranikust's
  own form (`user.overlay.opaque=y`, 0/0 whiteouts), `layers/` being written
  by ranikust alone.

### Q-130: Where kaniko and BuildKit differ on a config edit, which one wins?

- **Answer (2026-10-30):** BuildKit for each: (1) a new `ENTRYPOINT` clears
  an inherited `CMD` unless the stage set one; (2) `EXPOSE` takes
  `tcp|udp|sctp` in any case, writes it in lower case and expands ranges
  into one key per port; (3) `HEALTHCHECK --start-interval` is kept; (4) an
  absolute `WORKDIR` stays as written, which BuildKit does too (the entry's
  claim that it cleans it was wrong). Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-131: Which instructions get a history entry, with which fields?

- **Changed (2026-11-01):** a step that changes nothing adds no layer,
  only its history entry with `empty_layer: true`
  ([ADR-0058](adr/0058-a-step-that-changes-nothing-adds-no-layer.md), Q-385).

- **Answer (2026-10-30):** BuildKit's: one entry per instruction,
  `empty_layer: true` for metadata instructions, a layer for `RUN`, `COPY`,
  `ADD` and `WORKDIR` other than `/`, no entry for `FROM` and `ONBUILD`, no
  author; `RUN`'s `created_by` carries the build args. The `comment` string
  is the implementing branch's choice. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-132: Are empty values of a base config written back?

- **Answer (2026-10-30):** option (b), plus BuildKit's spec round-trip:
  empty strings, `false`, empty lists and maps are dropped, and so are
  fields outside the OCI and Docker image specs (`Hostname`, `Domainname`,
  `Tty`, `AttachStd*`, `OpenStdin`, `StdinOnce`, `Image`, `container`,
  `container_config`, `docker_version`). Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-150: Is an extracted layer keyed by its diff ID alone?

- **Answer (2026-10-30):** option (b): a layer whose extraction copied a
  hardlink target from a lower is keyed by its diff ID and the chain below
  it; other layers keep the diff-ID key. BuildKit keys every unpacked layer
  by chain ID. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-151: How does the mount(2) fallback fit many layers?

- **Answer (2026-10-30):** option (b), the containerd way: mount from a
  short-lived thread that does `unshare(CLONE_FS)` and `chdir` into `l/`,
  with relative lower names; no child process. The entry's "only in a forked
  child" was wrong. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-160: Are base layers rewritten under `--reproducible`?

- **Answer (2026-10-30):** option (a): base layers are kept as pulled, and
  their history keeps its times; only the layers built after the base are
  rewritten, as BuildKit's `rewriteRemoteWithEpoch`. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-161: Which fixed time does `--reproducible` write?

- **Answer (2026-10-30):** option (a): `SOURCE_DATE_EPOCH` when set, else
  the Unix epoch (equal to `SOURCE_DATE_EPOCH=0`). As BuildKit, times above
  that epoch are clamped to it and older ones are kept, instead of every
  time being set. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-180: Does VOLUME create its directories?

- **Answer (2026-10-30):** option (a): `VOLUME` only records the config, and
  an empty name is refused, as BuildKit. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-181: How is `RUN --mount=type=cache,sharing=` enforced?

- **Answer (2026-10-30):** enforce `sharing=` as BuildKit: `shared` is one
  cache for all; `locked` waits until the cache is free; `private` takes a
  new empty cache when the existing one is busy (not a copy). `uid`, `gid`
  and `mode` apply when the cache is created and are part of its key. The
  entry's "private = own copy" was wrong. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-182: Which `RUN --mount=type=secret` options are honoured?

- **Answer (2026-10-30):** a build-private copy of the secret, mode `0400`
  owned by root by default, with `mode`, `uid` and `gid` honoured, and
  `env=` supported, as BuildKit. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-184: May a Dockerfile's `RUN --network=host` lift `--network=none`?

- **Answer (2026-10-30):** no, unless the operator allows it: `RUN
  --network=host` needs `--allow network.host` (BuildKit's entitlement),
  which then lifts `--network=none`. Under the default host network
  (`ADR-0010`) nothing extra is needed. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-185: Does an image without `PATH` get one in its config?

- **Answer (2026-10-30):** yes: any Linux config without `PATH` gets
  `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`, as
  BuildKit; this replaces the interim that added it only for `scratch`.
  Recorded in [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-186: Does a cancel kill a running `RUN`?

- **Answer (2026-10-30):** yes: a cancel kills the running `RUN` with
  SIGKILL (its PID 1, and with it the PID namespace), as BuildKit's
  executor. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-187: How does the W3 digest check treat the workload's `du` output?

- **Answer (2026-10-30):** option (b): the acceptance criterion compares
  `--jobs 1` with the default (equal on 2026-10-30); the e2e suite compares
  the W3 images across `--jobs` by flattened content with the workload's
  `du`-derived files on its noise list, because those files depend on disk
  block allocation, not on ranikust. Every W3 stage writes `du`, so the
  list is `/out/a.txt`, `/out/b.txt`, `/out/c.txt` and `/out/all.txt`
  (`/out/b.txt` was seen to differ too; corrected on the branch that made
  the harness change).
  `nextjs-kaniko` is a separate project and is not changed here.

### Q-200: How does the e2e suite's `ranikust` reach a plain-HTTP registry?

- **Answer (2026-10-30):** option (a), the recommendation:
  `--insecure-registry <host>` (repeatable) with kaniko's meaning
  (`ADR-0011`), HTTPS first and plain HTTP when it fails
  (`feat/rnk-builder-executors`).

### Q-045: How are git contexts fetched and addressed?

- **Answer (2026-10-30):** option (c): a built-in `gix` backend by default,
  an external `git` with `--git-backend=external`, its binary, extra
  configuration (`--git-config`, passed through `GIT_CONFIG_*`), extra
  global options (`--git-arg`) and the host configuration
  (`--git-host-config`) configurable; kaniko's `--git` and `GIT_*`
  variables kept; the external `git` isolated and hardened as BuildKit's,
  plus hooks and `fsmonitor` off; both fragments, `#ref:subdir` and
  `#ref#commit`. Decided by the maintainer; recorded in
  [ADR-0035](adr/0035-git-contexts-built-in-or-external-git.md).

### Q-183: What does `RUN --network=none` do without `CAP_NET_ADMIN`?

- **Answer (2026-10-30):** (a) by default: refused where the build
  process cannot bring up `lo`; `--allow network.none-userns` opts in to
  (c), a network namespace owned by a user namespace of its own, with a
  seccomp filter on the `RUN` (a denylist; rtnetlink stays open), since its
  root holds every capability in that user namespace. Phase 3 moves the opt-in to running the `RUN`
  inside the user namespace. Recorded in
  [ADR-0033](adr/0033-network-none-loopback-through-a-user-namespace.md).

### Q-341: May kaniko mode build on `/` in a container where root may mount?

- **Answer (2026-10-31):** "Q341 chọn A": option (a). Kaniko mode on `/` is
  refused where root may mount, and also where `/proc/sys` is not
  read-only (`statvfs`), pointing to overlay mode; host devices given with
  `--device` stay the operator's responsibility (documented). Option (b),
  a private mount namespace with Docker's read-only and masked paths, is
  not built. Recorded in [ADR-0054](adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md).
