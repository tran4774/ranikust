---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0124: A `--chown` owner is keyed as written, and the chain of keys fixes the root it resolves in

## Context and Problem Statement

[ADR-0113](0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)
keys each file a `COPY` or `ADD` writes by its "uid and gid after `--chown`".
[ADR-0114](0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)
computes every key before any stage runs, when the stage's root, and so its
`/etc/passwd` and `/etc/group`, does not exist yet. The build resolves `--chown`
in that root (`rnk_rootfs::lookup_owner`, which tries a name first, even for a
numeric value), so the key plan
([#126](https://github.com/tran4774/ranikust/issues/126)) cannot hold the ids as
ADR-0113 words it. The same lookup happens for the directories a `WORKDIR` makes
for the active `USER`, which ADR-0113's list keys by the `USER` string.
[#168](https://github.com/tran4774/ranikust/issues/168) asks which owner the key
holds.

## Considered Options

The option letters are those of #168.

- (a) Key the owner as written, in the instruction, with the chain: the previous
  key fixes the root the name resolves in, as long as ADR-0114's stop rule holds,
  also across `FROM <stage>`. The key stays one computed before the build.
- (b) As BuildKit: key the content of the root's `/etc/passwd` and `/etc/group`
  (or the resolved ids) for a step with an owner name. Such a step can be looked
  up only once its root exists, which breaks the lookahead and the skipping of
  cached stages for the stages that hold one.
- (c) No key for a step with `--chown`: it and the rest of its stage always miss.
  The safest, but `COPY --chown=app` is common in real Dockerfiles and would end
  the stage's hits.

## Decision Outcome

The key of a `COPY` or `ADD --chown=<user>[:<group>]` holds the owner string as
written, not the ids it resolves to, and the previous key fixes the root the name
resolves in.

The maintainer's answer (2026-11-07: "Owner như viết, dựa vào chuỗi key (khuyến
nghị)", "The owner as written, relying on the chain of keys (recommended)") stood
for option (a) of #168. His comment on #168 adds that the answer also covers the
directories a `WORKDIR` creates for a named `USER`.

- **As written.** A numeric owner is keyed as written too, since the lookup tries
  a name first. `app` and `1000` are two keys even if they resolve to one id:
  only a miss.
- **Across `FROM <stage>`.** ADR-0114's stop rule (a stage uses its hits up to its
  first miss) applies across stages: a miss in the base stage is a miss for every
  step of a stage built on it. The key plan tells which stage a stage is built on.
- **`WORKDIR`.** The directories a `WORKDIR` creates for a named `USER` are keyed
  by the `USER` string, as ADR-0113 lists, and the chain fixes the root it
  resolves in.
- **Replaces** ADR-0113's wording "uid and gid after `--chown`" for a `COPY` or an
  `ADD`; the rest of its field list stands.
- **Security.** A hit carries ids from another `/etc/passwd` only if one key names
  two different lowers, the assumption every `RUN`'s key already makes.

### Consequences

- Good, because every key is still computed before any root exists, so the
  lookahead and the skipping of cached stages
  ([ADR-0114](0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md))
  work for a stage with `--chown`.
- Good, because it makes no assumption a `RUN`'s key does not already make.
- Bad, because two spellings of one owner are two keys, and a miss in a base stage
  ends the hits of every stage built on it.

## Evidence

- **[code]** `lookup_owner` reads the root's `/etc/passwd`, takes a name before a
  number, and reads `/etc/group` for a group (`crates/rnk-rootfs/src/user.rs:110-123`).
- **[external]** buildkit@cd8ee244: a named owner is keyed by selecting the
  destination's `/etc/passwd` and `/etc/group`, a numeric one by its number
  (`solver/llbsolver/ops/file.go:113-115`, `:291-318`).
- **[inferred]** The security argument is the key's chain: one key names one lower.
  Nothing measures how often a Dockerfile names an owner, or how many hits two
  spellings cost.

## More Information

Answers [#168](https://github.com/tran4774/ranikust/issues/168). Partly
supersedes ADR-0113: only its wording "uid and gid after `--chown`", which cannot
be computed before the build. It refines ADR-0114 (its stop rule across
`FROM <stage>`). Both stay accepted and are not rewritten.

Built by [#126](https://github.com/tran4774/ranikust/issues/126), the key plan.
[cache-and-store](../design/cache-and-store.md#cache-keys), the `rnk-cache` card
and the `rnk-builder` card say "as written" when it is built. No user-visible
change, so no `## Changelog` entry.
