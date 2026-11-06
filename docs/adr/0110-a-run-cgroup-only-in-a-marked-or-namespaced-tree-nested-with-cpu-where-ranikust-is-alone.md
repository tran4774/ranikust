---
status: accepted
date: 2026-11-05
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0110: A `RUN`'s cgroup is made only in a marked or namespaced delegated tree, nested with `+cpu` where ranikust is alone, after a start-up probe

## Context and Problem Statement

[ADR-0083](0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md)
(a) gives a full-sandbox `RUN`'s command a cgroup of its own, apart from its
init's and ended with `cgroup.kill`, "where a delegated cgroup v2 tree exists".
It leaves to the implementation how ranikust finds such a tree and what it does
when a write fails ([#33](https://github.com/tran4774/ranikust/issues/33)).
Each of those points changes behaviour:

- Root may write any cgroup on a host, including systemd units that were never
  delegated to it.
- An init cgroup with `cpu` beside the command's needs `cpu` enabled in the
  parent's `cgroup.subtree_control`, and a domain cgroup that holds processes
  cannot enable it.
- A probe, a `RUN`, or a killed ranikust can each fail or stop half-way.

Which trees count as delegated, which cgroups does ranikust make there, and what
happens on a failure?

## Considered Options

- **Delegated:** (a) any writable cgroup v2; (b) only a cgroup carrying
  systemd's delegation mark, or the root of ranikust's own cgroup namespace on a
  read-write cgroupfs; (c) only on an opt-in flag, as BuildKit's
  `BUILDKIT_SETUP_CGROUPV2_ROOT`.
- **Cgroups made:** (a) one controller-less cgroup for the command; (b) nesting:
  ranikust moves into a leaf, enables `+cpu`, and each `RUN` gets an init leaf
  and a command leaf; (c) (b) where possible, else (a).
- **Failures:** (a) a start-up probe of every step, the build as today when it
  fails, a `RUN` failed when one of its steps fails; (b) a failing `RUN` step
  only warns and the `RUN` goes on without a cgroup; (c) a marked tree whose
  probe fails refuses the build.
- **Leftovers after `SIGKILL`:** (a) none removed, documented; (b) removed by
  the next start when their owner is dead.
- **Scope:** the full sandbox only, or `Direct` on `/` too; kernels before 5.14.

## Decision Outcome

A full-sandbox `RUN` gets cgroups of its own only where ranikust's cgroup
carries systemd's delegation mark, or is the root of ranikust's own cgroup
namespace on a read-write cgroupfs, and the kernel has `cgroup.kill`. There,
ranikust nests with `+cpu` if it is alone in its cgroup and `cpu` is delegated.
Otherwise it gives the command one cgroup with no controller. A start-up probe
of every step makes the choice, and a failed probe leaves the build as it is
today. The maintainer chose these on 2026-11-05, on #33, picking the
recommended option of each question:

1. **What counts as delegated:** "Chỉ khi có dấu giao (khuyến nghị)", option
   (b). The cgroup ranikust starts in must carry systemd's `trusted.delegate`
   or `user.delegate` mark, or ranikust must be at the root of its own cgroup
   namespace with a read-write cgroupfs (a `--privileged` container). A cgroup
   that is only writable does not count, and neither does a mark on an ancestor.
   Refined below: a root ranikust trusts `trusted.delegate` alone.
2. **Which cgroups:** "Chia CPU nếu được, không thì hộp đơn (khuyến nghị)",
   option (c).
   - Where ranikust is alone in its cgroup and `cpu` is delegated, it nests: it
     moves into a leaf of its own and enables `+cpu`, and each `RUN` gets a
     cgroup with an init leaf and a command leaf.
   - Otherwise the command alone gets a cgroup with no controller, and the init
     stays in ranikust's cgroup. Refined below: the command runs in a
     `command` leaf of that cgroup.
   - Either way the choice made is logged.
3. **Failures:** "Thử trước lúc khởi động (khuyến nghị)", option (a),
   following
   [`design/sandbox.md#failure-handling`](../design/sandbox.md#failure-handling).
   - One start-up probe runs every step: create the cgroups, move a probe child
     in, write `cgroup.kill`, remove them.
   - If the probe fails, ranikust logs the step and errno and builds as it does
     today.
   - If a step fails during a `RUN` after the probe passed, that `RUN` fails,
     naming the step and errno.
4. **What a killed ranikust leaves:** "Để đó, ghi vào tài liệu (khuyến
   nghị)", option (a). Nothing removes the empty cgroups a `SIGKILL`ed
   ranikust leaves, or its leaf and `+cpu` when it nested; they stay until the
   unit or container ends, and the docs say so. A ranikust that ends normally
   removes them, and when it nested it moves back into its cgroup.
5. **Scope:** "Đồng ý (khuyến nghị)". Only the full sandbox is covered;
   `Direct` on `/` is unchanged, as are the probe's own `RUN`s. A kernel
   without `cgroup.kill` (before 5.14) counts as not delegated.

`cgroup.kill` is written when the `RUN` ends, when it is cancelled, and when
ranikust stops on a signal it handles, which cancels its `RUN`s. When nested,
the write reaches the init's leaf too; otherwise the init is killed as before.
No cgroupfs is mounted in the `RUN`, and it holds no capability in ranikust's
user namespace that could mount one. A `RUN` that may create a user namespace
can still mount a cgroupfs of its own, rooted at the cgroup its command is
in, and make cgroups below it, as many and as deep as the limits ranikust
sets allow; ranikust removes them with the `RUN`'s (below).

The command makes a cgroup namespace of its own, rooted at its cgroup, once
it is there and before anything the `RUN` chose runs. A `RUN` then reads
`0::/` in `/proc/self/cgroup`, as in a container with a private cgroup
namespace (Docker's and containerd's default on cgroup v2), where kaniko and
BuildKit `RUN`s run, and its init's cgroup relative to that (`/../init`
nested, `/../..` otherwise). Without it, a `RUN` read a path naming
ranikust's pid and the `RUN`'s number in start order, and a
`--reproducible` build that wrote it got another digest at another
`--jobs` or on another run (red-team, 2026-11-06), against the rule that
digests never depend on scheduling. The namespace takes `CAP_SYS_ADMIN`
(out of the bounding set already) and comes before the command's seccomp
filters, which refuse it, so the init forks the command before either
installs them, and each gives up the capability after its own (the init
every capability, the command all but Docker's defaults), both before the
go. A failure fails the `RUN`, and the start-up probe makes one too (3).

While it was implemented, the maintainer answered three more questions on
2026-11-05, on #33:

- **systemd before 251:** "Sửa theo kết quả đo (khuyến nghị)". T2 found that
  Ubuntu 22.04's systemd 249 marks a delegated scope, so the Consequences and
  Evidence below follow the measurement. The decision is unchanged.
- **A process that joins ranikust's cgroup before `+cpu`:** "Chấp nhận, đúng
  như ADR (khuyến nghị)". The kernel refuses `+cpu` (`EBUSY`), so the start-up
  probe fails as (3) says and no `RUN` gets a cgroup. There is no fallback to
  a cgroup for the command alone.
- **The T2 harness's network check on a `unit`-only run
  ([#98](https://github.com/tran4774/ranikust/issues/98)):** "Sửa nhỏ ở nhánh
  riêng (khuyến nghị)". It gets a small fix on a branch of its own.

After host-safety's audit of the implementation, the maintainer answered three
more on 2026-11-05, on #33:

- **Which delegation mark is trusted:** "Root chỉ tin trusted.delegate (khuyến
  nghị)", refining (1). A root ranikust decides on `trusted.delegate` alone,
  which only `CAP_SYS_ADMIN` in the initial user namespace can set.
  `user.delegate` needs only write access to the cgroup's inode, which a root
  `RUN` with a cgroupfs of its own has where it shares ranikust's cgroup; it
  counts only for a ranikust that is not root and owns the cgroup (rootless,
  [#79](https://github.com/tran4774/ranikust/issues/79)).
- **Cgroups a `RUN` makes below its own:** "Xóa cả cây con, sửa tài liệu
  (khuyến nghị)". ranikust removes a `RUN`'s cgroups with every cgroup below
  them, the deepest first, each opened by descriptor relative to its parent,
  at most 4096 cgroups and 32 levels below the `RUN`'s. Past that bound the
  removal fails, so the `RUN` fails, and the empty cgroups stay. The default
  seccomp profile of
  [ADR-0103](0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md),
  Docker's
  ([ADR-0109](0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)),
  refuses the `RUN` the user namespace this takes, except under
  `--seccomp=unconfined` **[measured]**.
- **The root of a cgroup namespace on a read-write cgroupfs:** "Giữ quy tắc,
  ghi vào tài liệu (khuyến nghị)". It still counts as delegated, so the rule
  also accepts trees nobody delegated: root's own `unshare -C` with cgroup2
  remounted read-write, and a systemd 257 unit with
  `ProtectControlGroups=private` and no `Delegate=` **[inferred]** (from
  systemd's documentation, not tested).

Once that audit's findings were fixed, the maintainer answered one more on
2026-11-05, on #33:

- **Limits on the cgroups below a `RUN`'s:** "Đặt ngay trong PR này (khuyến
  nghị)", set in this change. Right after ranikust makes a `RUN`'s cgroup,
  before any process joins it, it writes `cgroup.max.descendants` (4095) and
  `cgroup.max.depth` (32) there: what the removal above goes through, the
  `RUN`'s own cgroup aside, so the kernel refuses a `RUN` more cgroups than
  ranikust removes. A failed write fails the `RUN`. These are the only limits;
  no resource limit (CPU, memory, pids) is set.

After host-safety's audit of the limits, the maintainer answered one more on
2026-11-05, on #33:

- **Where the single layout runs the command:** "Đưa lệnh vào R/command
  (khuyến nghị)". In the single layout too, each `RUN`'s cgroup gets a
  `command` leaf with no controller, and the command runs there, as nested.
  A cgroup namespace is rooted at the cgroup its maker is in, and the limits
  must sit on a cgroup above every cgroup namespace a `RUN` can make, so that
  no cgroupfs it can mount shows them, with or without `nsdelegate`. With the
  command in `command`, that holds in both layouts.
  It is still one cgroup for the command with no controller, as (2) says;
  only where the command runs in it changes.

After red-team's tests of these cgroups, the maintainer answered one more on
2026-11-06, on #33:

- **How long the init takes to die when nested:** "Ghi là rủi ro còn lại
  (khuyến nghị)", accepted as a residual risk. In the nested layout a `RUN`
  can lengthen its init's death delay after ranikust is killed, up to the
  delay of the layout without nesting
  ([threat model, section 6](../security/threat-model.md#6-accepted-risks)).

### Consequences

- Good, because an environment that delegated nothing (root on a plain host, a
  systemd unit without `Delegate=`, a default Docker container, most pods)
  keeps today's behaviour, and systemd's rule that only a delegated subtree may
  be written by its delegatee holds.
- Good, because where ranikust nests, the init has a leaf of its own beside
  the command's; in T2, in the spinner cases of threat-model section 6 (8
  and 32 spinners), the namespace died within 4.1 ms of the death signal
  over five runs, against 7.6 s and 30.6 s without nesting **[measured]**
  (Evidence). One write to the `RUN`'s `cgroup.kill` ends
  the init and the command.
- Bad, because those figures are not a bound: in the nested layout a `RUN`
  can still lengthen its init's death delay after ranikust is killed, up to
  the delay without nesting, a residual risk (above).
- Good, because a `RUN` reads the same cgroups whatever ranikust's pid and
  the order its `RUN`s start, so its layers do not depend on them.
- Bad, because nesting moves ranikust's own process and enables `+cpu` in the
  delegated cgroup while it runs, and ranikust's own threads then share one
  leaf's CPU against the `RUN`s. The cost on W1–W3 is not measured.
- Bad, because without nesting the init has no cgroup of its own, so after a
  `SIGKILL` of ranikust, which leaves no one to write `cgroup.kill`,
  threat-model section 6's delay remains there.
- Bad, because a `Delegate=yes` unit whose cgroup systemd does not mark gets
  no cgroup; only a privileged container's cgroup namespace does. The systemd
  of Ubuntu 22.04 (249) and 24.04 (255) marks it **[measured]**; which
  releases do not is not known.
- Bad, because a `SIGKILL`ed ranikust leaves empty cgroups in the delegated tree
  until the unit or container ends.
- Bad, because a `RUN` that may create a user namespace can make cgroups below
  its own, as many as their removal goes through. A `RUN` that gets no cgroup
  leaves what it made in ranikust's cgroup, as before this ADR.
- Neutral, because a cgroup namespace's root counts as delegated even where no
  one delegated it; whoever made the namespace chose to give ranikust that
  tree.
- Neutral, because ADR-0007's "cgroups are only read" still holds wherever no
  tree is delegated, as ADR-0083 says.

## Evidence

- [external] The kernel's cgroup v2 documentation
  (`Documentation/admin-guide/cgroup-v2.rst`):
  - Controllers are enabled for children through `cgroup.subtree_control`.
  - A non-root cgroup with processes cannot enable domain controllers ("no
    internal process constraint").
  - `cgroup.kill` exists in non-root cgroups since Linux 5.14.
- [measured] On the development host (systemd 259, kernel 7.0), 2026-11-04,
  read-only: `user@1000.service` carries `user.delegate=1`, and a scope under
  it that was not delegated carries no mark although the user owns it. So
  being writable, or having a marked ancestor, does not mean delegated.
- [code] BuildKit `buildkit@cd8ee2441`:
  - `executor/resources/monitor.go:247-286` nests only when
    `BUILDKIT_SETUP_CGROUPV2_ROOT` is set, which its own image does. It then
    moves every pid of the namespace root into `/init` and enables every
    controller; failures are logged and ignored.
  - A step's cgroup is made by runc, and a failure fails the step.
  - BuildKit never writes `cgroup.kill`.
- [measured] T2 in the test VM (Ubuntu 24.04, kernel 6.8, systemd 255,
  Docker 29.1.3), 2026-11-05, `crates/rnk-sandbox/src/spawner/cgroup_tests.rs`:
  - Nested, in the spinner cases of threat-model section 6 (8 and 32
    spinners), the namespace died 1.0 to 3.2 ms and 1.6 to 4.1 ms after the
    death signal, over five runs (`a_nested_tree_shortens_the_spinner_cases_init_death`);
    without nesting, 7.6 s and 30.6 s (threat-model section 6).
  - A cancelled `RUN` returned 0.22 to 0.33 ms after the cancel, nested and
    single, in a scope and in a container.
  - After a `SIGKILL` of ranikust, systemd removed the delegated scope, with
    ranikust's leaf and a `RUN`'s cgroup, 50 ms later; a `--privileged`
    container's cgroup was gone once `docker kill` returned
    (`a_killed_ranikusts_cgroups_go_with_its_unit_or_container`).
  - `docker exec` into a nested `--privileged` container ran (exit 0) in
    ranikust's leaf (`0::/ranikust-1`), the cgroup of the container's PID 1.
- [measured] T2 in the test VM with each of its images, 2026-11-05
  (`inner_marks` in `cgroup_tests.rs`): a `Delegate=yes` scope carries
  `trusted.delegate=1` and no `user.delegate` under the Ubuntu 22.04 image
  (`RANIKUST_VM_RELEASE=jammy`: kernel 5.15, systemd 249.11-0ubuntu3.23),
  and both marks under the 24.04 image (systemd 255); ranikust nests under
  both.
- [measured] T2 in the test VM, 2026-11-05 (`cgroup_tests.rs`), after the
  answers that followed host-safety's audit. Under the Ubuntu 22.04 image a
  root `RUN` mounted a cgroupfs of its own in a user namespace and set
  `user.delegate=1` on ranikust's undelegated scope, which ranikust still
  found not delegated
  (`red_team_a_run_cannot_forge_a_delegation_mark_root_trusts`); a `RUN`
  that made cgroups below its own, with a process in one, left none, nested
  and single, in a scope and in a privileged container. Under the 24.04
  image AppArmor kept both `RUN`s from mapping their user namespace (`EPERM`
  on `uid_map`).
- [measured] T2 under the Ubuntu 22.04 image, 2026-11-05 (`cgroup_tests.rs`),
  with the limits set, in a scope and in a privileged container, and in a
  scope with the VM's cgroupfs remounted without `nsdelegate`
  (`run_cgroup_limits_on_a_cgroupfs_without_nsdelegate`). A `RUN` with a
  cgroupfs of its own made cgroups below its namespace's root up to the
  limits and got `EAGAIN` past them, then tried one more cgroup and one
  level deeper.
  - Before the single layout's `command` leaf, the single layout's limits
    did not hold; nested, 31 levels and 4062 more below `command` (4095 with
    `init` and `command`), and both tries got `EAGAIN`.
  - With the leaf, the limits held in both layouts, `nsdelegate` or not: 31
    levels, 4062 more nested and 4063 single, and both tries got `EAGAIN`
    everywhere.
  - Every cgroup and process was removed with the `RUN` each time.
- [measured] T1 (rootless podman `--privileged` on the development host,
  kernel 7.0, nested), 2026-11-06, the `run-cgroup` smoke case: two stages
  that write `/proc/self/cgroup` and `/proc/1/cgroup` to a file. Before the
  command's cgroup namespace, a `--reproducible` build of that shape gave
  another digest at `--jobs 1` and 4; with it, four builds at `--jobs 1` and
  4 gave one digest, and every `RUN` read `0::/` and `0::/../init`.
- [measured] T1 (rootless podman `--privileged` on the development host,
  kernel 7.0, nested), 2026-11-06, the `run-cgroup-escape` smoke case:
  every `/proc/<pid>/cgroup` the `RUN` could read was `0::/` or
  `0::/../init`; a `setns` into the init's cgroup namespace was refused, as
  root and from a user namespace the `RUN` made; and a cgroupfs it mounted
  in a user namespace listed the `RUN`'s processes at its root and no
  cgroup below it.
- [measured] T2 in the test VM under both images (Ubuntu 24.04, kernel 6.8;
  22.04, kernel 5.15), 2026-11-06, `cgroup_tests.rs`, with the command's
  cgroup namespace: in a delegated scope and in a `--privileged` container,
  a `RUN` read `0::/` in `/proc/self/cgroup` and its init's cgroup as
  `0::/../init` nested and `0::/../..` single, and a later `RUN` read the
  same; a root `RUN` held Docker's capabilities alone and its init none.
  Under 22.04 the limits held as above, with `nsdelegate` and without, and
  every cgroup and process went with the `RUN`.
- [measured] T1 (rootless podman `--privileged` on the development host,
  kernel 7.0, nested), 2026-11-06, after the merge with ADR-0109. With the
  profile installed before the init's fork, the `run-cgroup` case's first
  `RUN` failed at its cgroup namespace (`EPERM`), which the profile
  refuses. With the fork first and the namespace before the command's
  filters, the `run-cgroup` case passed, in the kaniko leg too, and in
  `run-cgroup-escape` a `RUN` under the default profile read `0::/` and
  `0::/../init` and was refused a user namespace (`unshare`, `EPERM`),
  while an unconfined one made one and its cgroupfs showed only its own
  cgroup.
- [measured] T2 in the test VM under both images (Ubuntu 24.04, kernel 6.8;
  22.04, kernel 5.15), 2026-11-06, `cgroup_tests.rs`, after the same
  change: in a delegated scope and in a `--privileged` container, nested
  and single, a `RUN` under the default profile read `0::/` and its init's
  cgroup as before and was refused a user namespace, which a later
  unconfined `RUN` of the same test made. Under 22.04 unconfined
  `RUN`s met the limits as above (31 levels, 4062 more nested and 4063
  single, `EAGAIN` past them), `nsdelegate` or not.

## More Information

Refines
[ADR-0083](0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md)
(a), and the order of
[ADR-0109](0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md):
the init's fork comes before the seccomp filters, which the init and the
command each install, the command after its cgroup namespace, as that ADR's
rule (every step that need not come after the profile comes before it)
asks. Implemented in `rnk-sandbox` (`cgroup.rs`) and described in
[`design/sandbox.md`](../design/sandbox.md). Task 8
([#79](https://github.com/tran4774/ranikust/issues/79)) checks the rootless case:
a user with a delegated tree. Revisit if the W1–W3 cost of nesting, measured
on request, is too high, or if a long-lived delegated unit builds up the
cgroups of killed builds.
