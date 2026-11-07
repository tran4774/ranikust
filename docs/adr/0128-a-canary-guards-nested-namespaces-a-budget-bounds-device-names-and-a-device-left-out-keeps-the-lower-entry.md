---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0128: A canary guards layers extracted under nested namespaces, a 1 MiB budget bounds the names of devices left out, and a device left out keeps the lower entry

## Context and Problem Statement

[#77](https://github.com/tran4774/ranikust/issues/77) extracts base layers
inside the build's user namespace
([ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)):
block and character devices are left out and counted as `nodes_skipped`, and so
are hardlinks to them
([ADR-0106](0106-extraction-in-a-user-namespace-skips-device-nodes-and-counts-them.md));
an extracted layer is keyed by its chain ID and the namespace's uid and gid maps
relative to its parent
([ADR-0104](0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)).
Building it raised three questions those ADRs leave open, put to the maintainer
in the question window.

1. **The key under nested namespaces.** A process cannot read the maps above its
   parent. In a rare case (ranikust in a rootless container, its state directory
   on a volume, the container's own subordinate range changing) the key stays,
   old layers are reused, their non-root owners show as the overflow id 65534 or
   as a wrong id, and a later `COPY --from` writes those owners into a new layer:
   the image then depends on the host. Host safety is not affected. BuildKit
   rootless has the same staleness.
2. **The names of the devices left out.** Extraction remembers them, so that a
   hardlink to one is left out with it. Memory must not grow with names the layer
   puts nowhere, so the names have a budget, and what a hardlink does past it
   needs deciding.
3. **A device left out at a path a lower layer also has.** Which entry does the
   build's root show there?

## Considered Options

The letters follow the order the options were asked in.

Point 1, the key under nested namespaces:

- (a) A canary per state directory and id map, as a separate task: entries owned
  by the first and the last id of each extent of the map, checked when the store
  opens; a mismatch refuses, with guidance to prune. It fails closed at any
  depth and also catches a volume chowned on disk. About a day of work plus an
  ADR, a change of the store format; a separate Phase 3 issue, and #77 merges
  with the limit documented ("Canary, làm thành task riêng (khuyến nghị)").
- (b) The same canary inside #77 ("Canary ngay trong #77").
- (c) Add the map of the set-up level to the key ("Thêm map của tầng set-up vào
  key"): one more level seen, automatic re-extraction, deeper levels still
  invisible.
- (d) Keep the key and document the limit ("Giữ nguyên, chỉ ghi docs").

Point 2, the budget for the names of left-out devices:

- (a) Keep it as built: a 1 MiB budget, one warning past it, and a hardlink to a
  device no longer remembered takes the normal hardlink path ("Giữ như hiện tại
  (khuyến nghị)").
- (b) Past the budget, refuse a hardlink whose target is not in this layer
  instead of copying from below ("Vượt ngân sách thì từ chối hardlink lạ").
- (c) Add a `--limit` ([ADR-0079](0079-resource-limits-with-a-limit-flag.md)) and
  refuse the image past the budget ("Thêm một `--limit`").

Point 3, a device left out at a path a lower layer has:

- (a) Keep the lower entry and say so in one sentence of the docs ("Giữ, ghi một
  câu vào docs (khuyến nghị)").
- (b) Write a whiteout at that path ("Ghi whiteout tại đường dẫn đó"): closer to
  overlay as root, but kaniko mode would change too, so it needs a new ADR that
  amends ADR-0106 and
  [ADR-0041](0041-live-root-extraction-skips-device-nodes.md).

## Decision Outcome

A canary per state directory and id map will guard extracted layers against a
change of the maps above the build's namespace, built by
[#183](https://github.com/tran4774/ranikust/issues/183) and not by #77; the names
of devices left out are remembered within a budget of 1 MiB a layer, with no
`--limit`; and a device left out leaves the lower entry at its path visible.

The maintainer's answers of 2026-11-07, one for each point, quoted in the
options above: (a) of point 1 ("Canary, làm thành task riêng (khuyến nghị)",
"A canary, as a separate task (recommended)"), (a) of point 2 ("Giữ như hiện tại
(khuyến nghị)", "Keep as it is (recommended)") and (a) of point 3 ("Giữ, ghi một
câu vào docs (khuyến nghị)", "Keep, put one sentence in the docs (recommended)").

- **Point 1: the canary.** ADR-0104's key stands: the chain ID and the
  namespace's maps relative to its parent. Its claim that the key also covers a
  rootless user whose ranges change holds for the namespace's own map, which the
  process reads; it does not hold for a change above the parent. The canary is
  the guard, per state directory and map: an entry owned by the first and by the
  last id of each uid and gid extent, checked when the store opens against the
  owners the namespace sees, with a mismatch refusing and naming the way out
  ([#183](https://github.com/tran4774/ranikust/issues/183) names
  `ranikust prune --all`,
  [ADR-0105](0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md)).
  Until #183 is built, #77 merges with the limit documented.
- **Point 2: the budget.** A layer's remembered names of devices left out cost
  their length plus 64 bytes each, within 1 MiB, about 14 000 devices with
  short names in one layer; real images hold none to a few dozen. Past it, one
  warning that names the budget, and a hardlink to a device whose name came past
  the budget takes the normal hardlink path: it is linked to what the layer has
  at that name, else it becomes a copy of the nearest lower layer's entry there,
  else the extraction fails and nothing is published. The result is
  deterministic, since the budget counts names alone and depends on the layer
  only, and is what BuildKit rootless always does. Only a crafted image reaches
  it.
- **Point 2, a budget and not a `--limit`.** No real image reaches the budget,
  and [ADR-0079](0079-resource-limits-with-a-limit-flag.md)'s limits are for
  inputs real images can reach, so an operator never needs to raise it; the
  warning names the budget.
- **Point 2 refines ADR-0106.** Its first consequence, "extraction never fails
  on an image holding device nodes", no longer holds past the budget: a crafted
  layer can be refused.
- **Point 3: the lower entry stays.** Where a layer holds a device at a path a
  lower layer also has, the device is left out and the lower entry stays visible
  in the build's root. Overlay as root shows the device and hides the lower
  entry, so the root differs from the image there. The docs say so in one
  sentence.
- **Host safety is not affected** by any of the three.

### Consequences

- Good, because the canary fails closed at any nesting depth, and it also catches
  a volume chowned on disk, which neither (c) nor (d) of point 1 does.
- Good, because #77 merges without a change of the store format, which is #183's
  task.
- Bad, because until #183 is built the rare case of point 1 is only documented:
  an image can depend on the host's subordinate range.
- Good, because the names of devices left out take a bounded memory, whatever a
  layer holds, and the result past the budget is the same on every run.
- Bad, because past the budget a crafted layer can be refused, or a hardlink to a
  device becomes a copy of a lower file: an exception to ADR-0106's "never
  fails", which only a crafted image reaches.
- Neutral, because a device left out at a path a lower layer has shows the lower
  entry, as kaniko mode, kaniko and BuildKit rootless do; only overlay as root
  differs.
- Bad, because in the build's root such a path differs from the image, which
  keeps the device: a `RUN` or a `COPY --from` sees the lower entry there.
- Bad, because the names are remembered per layer: a hardlink to a device a
  lower layer left out takes the normal hardlink path too, so it copies a deeper
  layer's entry at that name, or the layer is refused. Only a crafted layer
  links into a lower one.

## Evidence

Nothing is measured here.

- **[external]** BuildKit rootless keys unpacked snapshots by chain ID alone
  (`buildkit@cd8ee244 cache/refs.go:1420-1447`, `snapshot/snapshotid.go:16-18`),
  so the staleness of point 1 is not ranikust's alone.
- **[external]** containerd's archive apply, as BuildKit vendors it, remembers no
  names of skipped devices
  (`buildkit@cd8ee244 vendor/github.com/containerd/containerd/v2/pkg/archive/tar.go:795-807`),
  so a hardlink to a device past ranikust's budget is handled as BuildKit
  rootless always handles one. It skips block and character devices before
  clearing the path (`tar.go:251-262`, `:307-313` there), so the lower entry
  stays.
- **[external]** kaniko's `ExtractFile` has no case for character, block or FIFO
  entries (`kaniko@44aa7ddc0 pkg/util/fs_util.go:439-581`), so the lower entry
  stays.
- **[code]** kaniko mode skips a device before it clears the path
  ([ADR-0041](0041-live-root-extraction-skips-device-nodes.md)).
- **[code]** The budget is `SKIPPED_NAMES_MAX` (1 MiB) and `SKIPPED_NAME_COST`
  (64 bytes a name), charged in `Extractor::remember_skipped`, all in
  `crates/rnk-rootfs/src/extract.rs`.
- **[inferred]** That a process cannot read the maps above its parent, and that
  real images hold none to a few dozen devices, come from the question as put to
  the maintainer; nothing measures them.

## More Information

Raised while building [#77](https://github.com/tran4774/ranikust/issues/77), part
of [#41](https://github.com/tran4774/ranikust/issues/41). Refines ADR-0104 (its
key, point 1: decided here, built by #183) and ADR-0106 (points 2 and 3); both
stay accepted and their bodies are not rewritten. Related: ADR-0099, ADR-0041,
ADR-0079, ADR-0105.

Follow-ups:

- [#77](https://github.com/tran4774/ranikust/issues/77): the docs of the limit of
  point 1 in [cache-and-store](../design/cache-and-store.md#store-layout), the
  budget and its warning, and the one sentence of point 3 in
  [rootfs](../design/rootfs.md#extraction) and
  [overlay-snapshot](../design/overlay-snapshot.md).
- [#183](https://github.com/tran4774/ranikust/issues/183): the canary, a T1 test
  in a nested rootless Podman, T0 tests of the check, `host-safety` and
  `red-team`; then cache-and-store, the `rnk-store` and `rnk-snapshot` cards and
  the threat model's T13 row. Its refusal is user-visible, so its pull request
  has a `## Changelog` entry.

This ADR is a record only: no code and no user-visible change.
