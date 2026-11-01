---
status: proposed
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0060: `COPY` leaves special files out where `RUN` has no `nodev`; `--parallel-stages` held back

## Context and Problem Statement

[ADR-0041](0041-live-root-extraction-skips-device-nodes.md) makes kaniko
mode's extraction into a live root create no device node or FIFO, as
kaniko. `COPY` (from the context, a stage or an image) still copied them,
with `mknodat` as root. That is harmless only where the next `RUN` enters
the root `nodev`: the full sandbox binds the root `nodev`
(overlay mode, and kaniko mode off `/` since
[ADR-0059](0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md)),
which also leaves `FROM <stage>` copies without them. A `Direct` `RUN` on a
container's `/` has no such remount, and neither would the bare `chroot`
that [ADR-0014](0014-kaniko-mode-sequential-by-default.md) planned for
`--parallel-stages`.

Phase 2's task 10 was to build `--parallel-stages`. Since ADR-0014,
[ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md) and
[ADR-0056](0056-kaniko-mode-state-on-disk-behind-a-run-fence.md) keep the
state directory, the store and the credentials out of a kaniko-mode
`RUN`'s reach on `/`. A bare `chroot` per stage needs named stage
directories on `/`, and a `RUN` that keeps `CAP_SYS_CHROOT` (Docker's
default set) escapes it to `/` (experiment 12), reaching all of that.

## Considered Options

- `COPY`: (a) keep device nodes, FIFOs and sockets in every mode, as
  BuildKit; (b) leave them out wherever the `RUN` enters the root without
  `nodev`; (c) leave them out in every mode.
- `--parallel-stages`: the options of Q-400.

## Decision Outcome

- `COPY` follows (b): with `Isolation::Direct` (kaniko mode on `/`, and
  any future isolation without the `nodev` bind) it leaves device nodes,
  FIFOs and sockets out (`rnk_rootfs::CopyOpts::skip_special`), as
  extraction does there (ADR-0041); with the full sandbox it keeps them, as
  BuildKit. In kaniko mode the image then never holds a device node a
  `RUN` did not make in its own layer.
- `--parallel-stages` is recognised and refused with exit 2 and the reason
  (ADR-0049's rule for flags that would change nothing, or that cannot be
  honoured), whatever the environment, until Q-400 is answered
  [open: Q-400]. Outside a container it would add nothing (stages already
  run in parallel, ADR-0059); on a container's `/` none of the designs so
  far keeps ADR-0051's guarantees. ADR-0014's opt-in, ADR-0019's
  `JAVA_TOOL_OPTIONS` and ADR-0038's fake `/proc` (Q-037) wait with it.

### Consequences

- Good, because no `COPY` can hand a `Direct` `RUN` a device node that
  ranikust created as root outside any sandbox, and kaniko mode's images
  hold the same kinds of entries whether a file came from a base layer or
  a `COPY`.
- Good, because kaniko mode on `/` keeps the state out of every `RUN`'s
  reach; no flag reopens it.
- Bad, because a context or stage holding a FIFO or a device node is copied
  without it in kaniko mode on `/` (BuildKit and overlay mode keep it).
- Bad, because builds in containers that forbid mounts get no parallel
  stages; experiment 12 measured 21–44 % on W3 for them.

## More Information

- [Q-400](../open-questions.md#q-400-how-does---parallel-stages-run-stages-at-once-on-a-containers--without-mounts)
  holds the hardened-`chroot` design and what must be measured first.
- Host-safety review of task 9 raised the `COPY` gap (2026-11-01).
