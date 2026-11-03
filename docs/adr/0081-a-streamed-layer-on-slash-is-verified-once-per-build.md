---
status: proposed
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0081: On `/`, a streamed base layer is downloaded and verified once per build, and kaniko mode decompresses a layer beside its extraction

## Context and Problem Statement

In kaniko mode on a container's `/`, every base layer is streamed from its
registry (ADR-0055) and, by that ADR's amendment of 2026-11-01, spooled to
an anonymous file of the store, its compressed digest and diff ID verified,
before it is extracted. The spool was made for one extraction and dropped
after it. A build whose stages share a base therefore downloaded and
decompressed every layer of it twice per stage: once to verify, once to
extract. On W2, three stages `FROM` one node base, that was 15 downloads
and 30 decompressions of the base's layers.

The extraction itself decompressed on the thread that writes the files, so
a layer took the time of its decompression plus the time of its writes.

The Phase 2 acceptance run (ultra, 2026-11-03, `main` `3992fde`) found
kaniko mode on `/` 3.9 % slower than kaniko on W2 (15.81 s against
15.21 s, all 5 rounds). Its metrics put the difference in preparing the
stage roots: 3.73 s for `rnk_snapshot.prepare` against kaniko's 1.05 s of
`base.unpack`. Kaniko keeps each base layer's decompressed tar for the
later stages on the same base (`FF_KANIKO_REUSE_BASE_ROOTFS`), so it
inflates a base once, and its pipelined extraction inflates up to four
layers ahead of the one it extracts.

## Considered Options

- Keep each streamed layer's verified compressed spool while stages or
  `COPY --from` images the build made on the layer are still to read it.
- Keep a decompressed tar per layer, as kaniko: no second decompression,
  but the spool grows to the layer's uncompressed size, in memory under
  the `memfd` fallback, which ADR-0055's amendment rules out.
- Reuse the root itself between stages on one base, undoing what the
  previous stage changed: kaniko does not do it, and a missed change would
  leak one stage's files into the next.

And for the decompression:

- Decompress a layer on a thread of its own, a few chunks of plain tar
  ahead of its extraction.
- Decompress the next layers ahead, as kaniko: that needs their plain tars
  kept somewhere until they are extracted, the size the previous list
  rules out.

## Decision Outcome

On `/`, a streamed base layer's verified spool is kept for every stage
root and `COPY --from` image the build made on the layer, and goes with
the last such use.

- The snapshotter counts the uses as the builder announces them: one per
  `Snapshotter::streamed_layer` call, which the builder makes for each
  stage or image it pulls on the layer. A layer extracted more often than
  announced is spooled again; one announced but not extracted keeps its
  spool until the build ends.
- The spool stays the compressed blob, capped by its descriptor's size,
  and an anonymous file of the store that no `RUN` can open (ADR-0051).
  Every extraction from it still decompresses it and checks the diff ID
  on the way, as the extraction of a spool always did.
- Overlay mode and kaniko mode off `/` are unchanged: they do not spool.

In kaniko mode, on `/` and off it, a layer's extraction reads its plain
tar from a thread that decompresses the blob and computes the diff ID, at
most 4 chunks of 1 MiB ahead.

- The thread only reads the blob and decompresses it; every file is still
  written by the extraction, through `RootPath`, in the tar's order. The
  bytes extracted are the ones decompressed, in the same order.
- An error of the blob or of the decompression reaches the extraction as a
  read error of the same kind. An extraction that stops early drops its
  reader, the thread ends at its next chunk and is joined: no thread
  outlives the extraction of its layer, so none runs during a `RUN`.
- The decoder, and its `rnk_layer.decode` span, are made and dropped on
  the extraction's thread.

### Consequences

- Good, because a stage on a base an earlier stage of the build used no
  longer downloads it, and no longer decompresses it a first time to
  verify it.
- Good, because a layer's extraction takes the longer of its
  decompression and its writes instead of their sum.
- Bad, because the compressed layers of a shared base stay on the state
  directory's filesystem, or in memory under the `memfd` fallback, from
  the first stage on it to the last, instead of one layer at a time.
- Bad, because each extraction holds one more thread and up to 6 MiB of
  plain tar (4 queued chunks, the one being filled and the one being read).
- Neutral, because layers, digests and what reaches `/` are unchanged:
  the bytes extracted are the ones verified.

## Evidence

- `kaniko/root/tests.rs`
  `on_slash_a_layer_several_stages_were_made_on_is_downloaded_and_verified_once`:
  three stages on one streamed layer opened its source 3 times before and
  1 time after; `on_slash_a_spool_goes_with_the_last_use_the_build_announced`;
  `a_layer_decompressed_on_its_own_thread_reads_as_the_decoder_and_has_its_digest`;
  `a_corrupt_layer_fails_its_reader_and_a_reader_that_stops_early_ends_the_thread`.
- The kaniko behaviour comes from an upstream-oracle read of the kaniko
  fork at `44aa7ddc0` (`pkg/executor/base_reuse.go:19-44, 142-175`).
- Measured in the test VM (`just test-vm ultra` limited to W1–W3 and the
  kaniko-mode builders, 3 rounds, `--jobs` default), median wall seconds
  against the acceptance run (5 rounds, `main` `3992fde`); kaniko's own
  times are the same in both, within 0.02 s:

  | Workload | Builder | Before | After | kaniko |
  |---|---|---:|---:|---:|
  | W2 | kaniko mode on `/` | 15.81 | 14.61 (14.52–14.64) | 15.21 |
  | W2 | kaniko mode, host | 8.58 | 8.08 | |
  | W1 | kaniko mode on `/` | 20.00 | 19.64 | 20.49 |
  | W1 | kaniko mode, host | 18.58 | 19.16 | |
  | W3 | kaniko mode on `/` | 17.93 | 16.04 | 26.26 |
  | W3 | kaniko mode, host | 10.82 | 10.93 | |

  On W2 on `/`, the first part alone (3 rounds) gave 15.16 s against
  kaniko's 15.00 s: `rnk_layer.decode` spans 38 → 28, `pull_blob` 23 → 13,
  `rnk_snapshot.prepare` 3.73 s → 2.94 s. W1 and W3 on a host are within
  the spread of the acceptance run (18.51–19.82 s, 10.61–12.54 s): on W1
  `prepare` and `decode` fell by 0.37 s and 0.35 s, and the rounds' wall
  times follow the `RUN pnpm install` and the export of its layer, which
  this change does not touch. Layers are the same: the comparison with
  kaniko finds no difference other than its known noise, as before.

## More Information

Refines the spool lifetime of ADR-0055's amendment of 2026-11-01; relates
to ADR-0051 and ADR-0056 (anonymous files of the store).
