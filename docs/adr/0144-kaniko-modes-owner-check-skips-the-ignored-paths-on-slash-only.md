---
status: accepted
date: 2026-11-08
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0144: Kaniko mode's owner check skips the ignored paths on `/` only

## Context and Problem Statement

[ADR-0098](0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)
refuses an image with an owner the build's user namespace does not map, and
reads the owners before the layers are extracted.
Kaniko mode extracts a base layer with the
[ignore list](../design/kaniko-mode.md#ignore-list)
(`rnk_rootfs::extract_live`), which leaves out the paths it covers. A check
that read every entry refused an image whose only unmapped owners sit on
paths that no base-layer extraction writes.
[ADR-0137](0137-kaniko-mode-on-slash-checks-each-streamed-layers-owners-just-before-extracting-it.md)
left those owners open ("Not decided here", #209), and #209 asked for the
ignore list in the scan on `/`. The question put to the maintainer in the
question window, which has no `question` issue: record the rule as the
branch of #209 built it?

## Considered Options

The options weighed while building #209.

- (a) Skip the ignored paths on `/` only, where the extraction that follows
  skips them; off `/` check every path, the ones its extraction skips too
  (recommended, the branch).
- (b) Check every path everywhere, as before #209: an image is refused for
  owners on paths kaniko mode on `/` never writes.
- (c) Skip the ignored paths off `/` too: #209 decided only `/`, and off `/`
  the stricter check costs little (the stage's ignore list holds the fixed
  paths and `--ignore-path`, nothing mounted).

## Decision Outcome

In kaniko mode in a user namespace, the owner check skips the paths the
ignore list covers only on `/`, for a layer extracted there with that list;
a `COPY --from` image on `/`, and every layer off `/`, are checked on every
path, the ones the stage's extraction skips too.

The maintainer's answer (2026-11-08: "Ghi ADR như nhánh làm (khuyến
nghị)", "Record the ADR as the branch did it (recommended)") stood for
option (a).

- **On `/`, base and cached layers.** The spool pass of a streamed layer
  (`Spooled::of`), `Snapshotter::check_owners` for pulled blobs, and the
  check before a cached blob is applied (`apply_layer`) all skip what the
  build's ignore list covers, matched on the same cleaned path the
  extraction matches. An entry reached through a symlink is still noted
  **[code]**.
- **`COPY --from=<image>` on `/`.** It is unpacked into a directory of the
  state directory with no ignore list, so its owners are checked on every
  path. A spool or pulled blob that an earlier check passed for a base is
  read again for it, with no second download
  ([ADR-0081](0081-a-streamed-layer-on-slash-is-verified-once-per-build.md))
  **[code]**.
- **Off `/`** (a directory per stage, or another root): every path, the
  ignored ones too, for base and cached layers alike **[code]**.
- **Not changed.** Overlay mode's check (`scan_owners`, no ignore list);
  a `chown` that fails with `EINVAL` during extraction still refuses
  (ADR-0098).

### Consequences

- Good, because on `/` the check refuses only owners the extraction would
  set, so an image whose unmapped owners sit only on ignored paths builds
  in a rootless container.
- Good, because an image unpacked whole is still refused before any
  directory is made for it.
- Bad, because the check has two rules, and off `/` an image may be
  refused for owners its stage's extraction would skip.
- Neutral, because a `COPY --from` image on `/` whose spool a base's check
  passed costs one more decompression of that spool, in a user namespace
  only.

## Evidence

- **[code]** `rnk_rootfs::check_headers_and_owners` skips a path the given
  ignore list covers; `tests/red_team_owners.rs` shows an ignored path
  skipped and a symlink route still named.
- **[code]** T0 tests in `crates/rnk-snapshot`:
  `on_slash_the_namespace_s_ids_and_the_ignore_list_reach_the_spool_pass`
  (a base passes, a `COPY --from` image is refused, from its own spool, a
  base's or a pulled blob),
  `off_slash_the_owner_check_covers_the_ignored_paths`,
  `off_slash_kaniko_s_owner_check_covers_the_ignored_paths`, and
  `a_cached_blob_with_an_owner_the_namespace_lacks_is_refused_before_the_root_changes`
  (both snapshotters).
- No measurement: the cost of the second read of a spool is
  **[inferred]** from how the spool works.

## More Information

- Refines ADR-0098; answers the point ADR-0137 left "Not decided here"
  (#209), whose body is unchanged.
- Design: [modes and detection](../design/modes-and-detection.md#owners-the-user-namespace-does-not-map),
  [kaniko mode](../design/kaniko-mode.md#a-cache-hit),
  [rootfs](../design/rootfs.md#extraction).
- Revisit if off `/` refusals for owners on ignored paths show up, or if a
  `COPY --from` image on `/` comes to be unpacked with the ignore list.
