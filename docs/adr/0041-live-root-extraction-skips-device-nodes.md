---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer), by the rule to follow kaniko and BuildKit
---

# ADR-0041: Extraction into a live root creates no device node or FIFO

## Context and Problem Statement

Kaniko mode extracts base layers into the root it builds in
(`rnk_rootfs::extract_live`): the container's own `/`, or a `--workdir`
on a host. A base layer may hold block and character device nodes and
FIFOs. Created as root in a host workdir, a node with a real device number
gives whoever can reach the workdir access to a host device (Q-270).

## Considered Options

- (a) Refuse device nodes other than the 0/0 character device when the
  root is a host workdir.
- (b) Skip device nodes and FIFOs in every live root, as kaniko does.
- (c) Create them, as overlay mode's extraction into the store does.

## Decision Outcome

Chosen option (b), by the maintainer's rule to follow kaniko where it
decides: `extract_live` creates no block or character device node and no
FIFO, in `/` and in host workdirs alike, and counts them as
`nodes_skipped` on the `rnk_rootfs.extract` span. kaniko's `ExtractFile`
handles regular files, directories, hardlinks and symlinks only and skips
every other entry type (`kaniko@44aa7ddc0 pkg/util/fs_util.go:440-575`).

### Consequences

- Good, because a host workdir never holds a device node from an image.
- Good, because the root matches kaniko's, so a `RUN` sees what it sees
  under kaniko.
- Neutral, because the image keeps the nodes: its base layers are reused
  unchanged, and the walk sees no change where nothing was created.
- Bad, because a `RUN` that expects a FIFO or device node from the base
  image misses it, as under kaniko; `/dev` is on the ignore list anyway.

## More Information

Answers [Q-270](../open-questions.md#q-270-are-device-nodes-from-an-image-created-in-a-host-workdir-root).
Overlay mode's extraction into the store (`extract_overlay`) is not
changed: its layers are lower directories, never a root on the host that a
user reaches. Design: [kaniko-mode](../design/kaniko-mode.md#ignore-list).
