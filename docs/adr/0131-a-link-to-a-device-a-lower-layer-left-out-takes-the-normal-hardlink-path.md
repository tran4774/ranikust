---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0131: A hardlink to a device a lower layer left out takes the normal hardlink path

## Context and Problem Statement

[#77](https://github.com/tran4774/ranikust/issues/77) extracts base layers inside
the build's user namespace, where block and character devices are left out and
counted as `nodes_skipped`
([ADR-0106](0106-extraction-in-a-user-namespace-skips-device-nodes-and-counts-them.md)).
Within one layer, a hardlink to a device the same layer left out is left out
with it, while the layer remembers its name.
[ADR-0128](0128-a-canary-guards-nested-namespaces-a-budget-bounds-device-names-and-a-device-left-out-keeps-the-lower-entry.md)
bounds those names with a budget of about 1 MiB a layer, and past it a link to a
name not remembered takes the normal hardlink path of
[ADR-0012](0012-accept-divergence-between-modes.md): it links
what the layer has at that name, else copies from the nearest lower layer that
shows the name, else the layer is refused.

The set of remembered names belongs to one layer. So a hardlink in an upper
layer to a device a *lower* layer left out takes the same path: the lower layer
has nothing at that name, so the link copies a deeper layer's entry at that name
if there is one, and the layer is refused if there is none. Only a crafted image
reaches this, since Docker and BuildKit never emit hardlinks across layers
**[inferred]**. Overlay as root creates the device, so the link there succeeds.
The question, put to the maintainer on 2026-11-07, was whether to keep that.

## Considered Options

The letters follow the order the options were asked in.

- (a) Keep the refusal or the copy, and record it as a second exception to
  ADR-0106 next to ADR-0128's budget exception, with no code change
  ("Giữ từ chối/copy, ghi ADR (khuyến nghị)").
- (b) Remember the names of left-out devices across layers in the extracted
  layer's metadata, so that an upper layer's link is left out too. Closer to
  ADR-0106, but it adds code, store metadata and tests; it could be a separate
  task later.

## Decision Outcome

A hardlink in a layer to a device a lower layer left out keeps taking the normal
hardlink path of ADR-0012, with no code change: option (a) (the maintainer,
2026-11-07: "Giữ từ chối/copy, ghi ADR (khuyến nghị)", "Keep the refusal or the
copy, and record it in an ADR (recommended)").

- **What happens.** The link copies the entry a deeper layer shows at that name,
  if there is one, and the layer is refused if there is none: the extraction
  fails and nothing is published, so it fails closed.
- **A second exception to ADR-0106.** ADR-0128 made past-the-budget links the
  first exception to ADR-0106's "extraction never fails on an image holding
  device nodes"; this is the second. Like the first, only a crafted image
  reaches it.
- **Where it is pinned and described.** The T0 test
  `a_link_to_a_device_a_lower_layer_left_out_is_refused_or_copies_a_deeper_layer`
  in `crates/rnk-rootfs/src/extract/tests.rs` pins both outcomes;
  [rootfs](../design/rootfs.md#extraction) describes it.
- **Not covered.** Overlay as root is unchanged: it creates the device, and the
  link succeeds.

### Consequences

- Good, because there is no code to write or maintain, and the result fails
  closed.
- Bad, because a crafted image that overlay as root accepts is refused in the
  namespace, or gets a deeper layer's file at that path.
- Neutral, because option (b) stays possible later, as a separate task.

## Evidence

Nothing is measured here.

- **[code]** The remembered names are the extractor's own set, one for each
  layer (`devices_skipped` in `Extractor`, `crates/rnk-rootfs/src/extract.rs`),
  so a lower layer's names are not seen. A link whose target is neither in the
  layer nor remembered goes to the nearest lower layer that shows it, and fails
  with "is in no layer below, or hidden there" when none does.
- **[code]** The T0 test named above builds a lower layer that leaves out two
  devices, with a deeper layer that has a file at one of the names: the link to
  the device with no deeper entry is refused, and the link to the other holds
  the deeper file's bytes.
- **[inferred]** That Docker and BuildKit never emit hardlinks across layers
  comes from the question as put to the maintainer; nothing measures it.

## More Information

Raised while building [#77](https://github.com/tran4774/ranikust/issues/77), part
of [#41](https://github.com/tran4774/ranikust/issues/41). Refines ADR-0106 next to
ADR-0128 (its budget exception); both stay accepted and their bodies are not
rewritten. Related: ADR-0012.

This ADR is a record only: no code and no user-visible change. The code, the test
and [rootfs](../design/rootfs.md#extraction) already describe the behaviour.
