---
status: accepted
date: 2026-11-05
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0109: Seccomp profiles are compiled to BPF by `rnk-sandbox`'s own code, and the default profile is moby's

## Context and Problem Statement

[ADR-0103](0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md)
applies a default seccomp profile to every `RUN` with the full sandbox and
keeps `--seccomp=<file>` from
[ADR-0003](0003-dockerfile-trust-and-default-isolation.md), but no ADR says how
a profile becomes a filter or where the default profile comes from.
[ADR-0009](0009-x86-64-first-aarch64-ready.md) only says the syscall numbers come
from `libc` or `rustix` and the profiles are generated per architecture. Today
`rnk-sandbox` has one hand-written filter, ADR-0033's, built with a small
assembler. Which mechanism compiles a Docker JSON profile, which profile is the
default, and how do unsupported parts of a profile behave (raised while
implementing [#82](https://github.com/tran4774/ranikust/issues/82))?

## Considered Options

Mechanism:

- (A1) Grow ADR-0033's assembler (`rnk-sandbox`'s `seccomp.rs`) into a
  compiler from Docker's JSON format to classic BPF.
- (A2) `seccompiler` 0.5.0 (rust-vmm), used to compile only.
- (A3) libseccomp, the C library runc uses, through the `libseccomp` crate.

(A2) sets `PR_SET_NO_NEW_PRIVS` whenever it installs a filter, which ADR-0103
refuses, takes one match action per filter, so a profile with an action per
rule becomes several stacked filters that settle conflicts differently from
libseccomp, and does not read Docker's format. (A3) is LGPL-2.1, not on
`deny.toml`'s allow list, and needs a C build for the static musl binary
([ADR-0020](0020-static-musl-via-cargo-zigbuild.md)).

Default profile:

- (B1) moby/profiles seccomp v0.2.4's `default.json`, the copy BuildKit
  vendors, embedded verbatim and compiled by the same code as a `<file>`.
- (B2) A profile of ranikust's own.

(B2) drifts from `docker build` and needs every entry justified and kept.

## Decision Outcome

`rnk-sandbox` compiles Docker JSON profiles to classic BPF with its own code,
grown from ADR-0033's assembler, and the default profile is moby/profiles
seccomp v0.2.4's `default.json`, embedded verbatim: options (A1) and (B1),
with the defaults below (the maintainer, 2026-11-05: "Tự viết (khuyến
nghị)", write our own, as recommended; "Chép của Docker (khuyến nghị)", copy
Docker's, as recommended; "Đồng ý tất cả (khuyến nghị)", agree to all, as
recommended).

- **No new dependency** for the filter, and no `no_new_privs`: the profile is
  installed in the `RUN`'s child after the mounts and before the `capset` to
  Docker's default capabilities, while the child still holds
  `CAP_SYS_ADMIN` (the order is below).
- **ADR-0033's filter stays**, under `--seccomp=unconfined` too, and is
  installed before the profile (below).
- **`default` is one more profile:** the embedded `default.json` goes through
  the same parser and compiler as `--seccomp=<file>`, with a `NOTICE` entry.
- **A syscall name the architecture does not know** is skipped under
  `SCMP_ACT_ALLOW`, as runc does, and makes the profile refused (exit 2) under
  a stricter action.
- **Refused (exit 2):** `SCMP_ACT_NOTIFY`, `SCMP_ACT_TRACE`, a `listenerPath`
  and any flag ranikust does not know.
- **`--seccomp` is refused (exit 2)** wherever a `RUN` gets `Isolation::Direct`,
  which is kaniko mode on `/`, ADR-0103's case.
- **`serde` and `serde_json`** come into `rnk-sandbox` to read the profile;
  both are already workspace dependencies.

Answered by the maintainer on 2026-11-05 too, on
[#82](https://github.com/tran4774/ranikust/issues/82#issuecomment-6639346214),
for what the implementation raised:

- **A call of another architecture** (i386 and x32 on x86_64, arm on
  aarch64) gets `EPERM`: a 32-bit program in a `RUN` needs
  `--seccomp=unconfined` ("Giữ EPERM, ghi tài liệu (khuyến nghị)", keep
  `EPERM` and document it, as recommended).
- **A call `libc` gives no number** gets `ENOSYS` under a default action
  that refuses, and is allowed once `libc` numbers it ("Giữ ENOSYS (khuyến
  nghị)", keep `ENOSYS`, as recommended).
- **Two conditions on one argument and a rule with no names** are both
  refused (exit 2), with a message that names the rule (`syscalls[N]`)
  ("Từ chối cả hai (khuyến nghị)", refuse both, as recommended).
- **The `RUN`'s init joins its new session keyring before the profile**,
  which refuses `keyctl`, as runc does; ADR-0080 had the command, PID 2,
  join it ("Ghi vào ADR-0109 (khuyến nghị)", record it in ADR-0109, as
  recommended).

Answered by the maintainer on 2026-11-05 as well, on
[#82](https://github.com/tran4774/ranikust/issues/82#issuecomment-6649627040),
for what the host-safety review raised: a profile is the operator's and may
answer any call with any errno (0, a faked success, too, until the answer
below), so it must not be able to change what the set-up does.

- **Every set-up step that need not come after the profile comes before
  it** ("Đưa các bước bảo mật lên trước (khuyến nghị)", move the security
  steps first, as recommended). The child joins the `RUN`'s session keyring,
  drops the bounding set, clears the ambient and inheritable sets, makes the
  init not dumpable, starts its session and sets the umask, then installs
  ADR-0033's filter and the profile on top of it. After the profile come
  only the `capset` to the default sets, since the install needs
  `CAP_SYS_ADMIN`, and the init's and the command's own steps (the init's
  fork, which the command checks, its capability drop, the command's
  session and `USER` switch).
  A call both filters refuse now gets the profile's errno; what is refused
  stays the same. An `EINVAL` from a bounding-set drop ends the list only
  when `caps::last_cap()` could not read the kernel's and assumed 63;
  otherwise it fails the `RUN`.
- **An `EACCES` on the keyring join fails the `RUN` when its profile lets
  `keyctl`, `add_key` or `request_key` run** ("Lỗi khi hồ sơ cho phép keyctl
  (khuyến nghị)", fail when the profile allows `keyctl`, as recommended);
  under a profile that refuses all three, as `default` does, it is ignored
  as before. The keyring probe below replaces the test on `EACCES`.
- A failed install names the filter: the profile's failure points to
  `--seccomp=unconfined`, ADR-0033's to `--allow network.none-userns`.

Answered by the maintainer on 2026-11-05 as well, on
[#82](https://github.com/tran4774/ranikust/issues/82#issuecomment-6651694125),
after the host-safety review of that order:

- **A profile cannot fake a success** ("Từ chối errnoRet 0 (khuyến
  nghị)", refuse `errnoRet` 0, as recommended). An `SCMP_ACT_ERRNO` whose
  errno in the filter would be 0, a rule's `errnoRet` or the
  `defaultErrnoRet`, refuses the profile (exit 2), naming `syscalls[N]` or
  `defaultErrnoRet`. An errno above 4095 is refused too (the filter keeps
  only its low 16 bits), and a negative one is not a number the format
  takes. A profile can then let a call run, refuse it with an errno from 1
  to 4095 but 512 to 530 (below), or kill the `RUN`: the steps
  after it see a refusal, never a success that did not happen. A filter
  around ranikust, which the operator's runtime installs, may still fake
  one; ranikust does not control it.
- **Nor can it answer with the kernel's restart codes** (the maintainer,
  2026-11-05, after the host-safety review of the errno rule: "Sửa luôn, coi
  là phần của quyết định (khuyến nghị)", fix it now, as part of the decision,
  as recommended). An errno from 512 to 530, the `ERESTART*` codes and their
  neighbours in the kernel's `include/linux/errno.h`, in a rule's `errnoRet`
  or the `defaultErrnoRet`, refuses the profile (exit 2) as errno 0 does,
  checked on the same value: on 516 (`ERESTART_RESTARTBLOCK`) the kernel may
  run its restart block instead of returning the refusal, and that can
  succeed.
- **After a refused keyring join, the child looks for ranikust's session
  keyring before the profile** ("Thăm dò keyring (khuyến nghị)", probe the
  keyring, as recommended), with
  `keyctl(KEYCTL_GET_KEYRING_ID, KEY_SPEC_SESSION_KEYRING, 0)`, instead of
  deciding by the join's errno. If the lookup succeeds, the `RUN` fails as
  `Step::KeyringRefused` unless its profile refuses `keyctl`, `add_key` and
  `request_key`; if the lookup is refused too, the `RUN` goes on. This
  holds under `unconfined` as well. Docker's default profile around
  ranikust refuses `keyctl` whole, the lookup included, so a build in
  Docker runs as before. `Direct` keeps its errno rule (`EPERM`, `ENOSYS`
  and `EACCES` go on).

### Consequences

- Good, because the install stays under ranikust's control: setuid programs
  keep working, ADR-0033's filter stacks under the profile, and nothing new
  is linked.
- Good, because a profile, whatever it answers, cannot undo the bounding
  set, the session, the keyring or ADR-0033's filter.
- Good, because a profile can refuse a call but never fake its success:
  the steps after it see a refusal as one.
- Bad, because a Docker profile file that answers a call with errno 0, or
  with one from 512 to 530, is refused, though runc, through libseccomp,
  would install it [inferred].
- Bad, because the steps after the profile still trust the answers of a
  filter around ranikust, which ranikust does not control (threat model,
  section 6).
- Good, because `default` is what `docker build` and BuildKit apply, and a
  user's Docker profile file works unchanged within the limits above.
- Bad, because the compiler is security-critical code of ranikust's own (64-bit
  argument compares, an action per rule, long jumps), which needs its own
  tests: compiled filters run in a forked child, and moby's profile is checked
  against a table for x86_64 and aarch64.
- Bad, because moby's profile has to be re-vendored by hand when it changes.
- Neutral, because a profile that asks for what ranikust does not support is
  refused before the build starts, never weakened.

## Evidence

- Nothing is measured here.
- **[external]** `seccompiler` 0.5.0: `apply_filter` sets
  `PR_SET_NO_NEW_PRIVS` (`src/lib.rs:344`); one match action per filter
  (`src/backend/filter.rs:85-108`).
- **[external]** libseccomp is LGPL-2.1; `deny.toml` allows no LGPL licence
  **[code]**.
- **[external]** BuildKit `cd8ee2441a4c` vendors
  `github.com/moby/profiles/seccomp` v0.2.4 and applies its `default.json` to
  every `RUN` (ADR-0103's Evidence); the profile is an allowlist whose default
  action is `SCMP_ACT_ERRNO`.
- **[external]** `seccomp(2)`: installing a filter without `no_new_privs`
  needs `CAP_SYS_ADMIN` in the caller's user namespace; with several
  filters, every one runs, newest first, and the first-seen action of
  highest precedence wins, with its errno.

## More Information

Refines [ADR-0103](0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md)
(how its profile is built),
[ADR-0033](0033-network-none-loopback-through-a-user-namespace.md) (its
assembler and install point are shared) and
[ADR-0080](0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md)
(the init, not PID 2, joins the session keyring). Built by
[#82](https://github.com/tran4774/ranikust/issues/82), task 11 of
[#41](https://github.com/tran4774/ranikust/issues/41); the design is in
[sandbox](../design/sandbox.md).
