# rnk-context

> **Status:** skeleton · **Phase:** 1 (local directory) ·
> **Depends on:** `rnk-rootfs` · **Used by:** `rnk-builder` · **Path:** `crates/rnk-context/`

**TL;DR:** The build context from a local directory, tar, git or HTTPS, behind
an open trait. Every context is hostile input: archive and clone contents
reach disk only through `rnk_rootfs::RootPath`.

## Owns

- Providing the build context from a local directory, a tar archive, a git
  repository or an HTTPS URL.
- The open trait through which new context sources are added later.
- Locating the ignore file: `<Dockerfile>.dockerignore` before
  `<context>/.dockerignore`, as kaniko and BuildKit do.

## Must not

- Fetch from cloud object stores (S3, GCS, Azure) for now (ADR-0008).
- Interpret the Dockerfile or decide which files an instruction copies
  (`rnk-builder`); match ignore patterns itself (`rnk-ignore`).
- Reach a context path with `std::fs` on a joined host path (`rnk-rootfs`).

## API (planned)

```rust
pub enum ContextSpec { Dir(PathBuf), Tar(TarInput), Git(GitSpec), Https(Url) }  // [design] kaniko prefixes
pub fn parse_spec(arg: &str) -> Result<ContextSpec, Error>;  // [design] dir://, tar://, git://, https://; bare path = dir
pub trait ContextSource {                                     // [decided] open trait, ADR-0008
    fn open(&self, build: &BuildPaths) -> Result<Context, Error>;  // [open: Q-027] sync or async (HTTPS, git)
}
pub struct Context { pub root: RootPath, pub sub_path: Option<PathBuf> }  // [design] read-only view for COPY/ADD
// [design] a local directory is used in place; tar, git, HTTPS are unpacked into builds/<build-id>/context/
pub fn find_dockerignore(ctx: &Context, dockerfile: &Path) -> Result<Option<Vec<u8>>, Error>; // [design] <Dockerfile>.dockerignore first
```

## Invariants

- Nothing from a tar, git or HTTPS context is written except through
  `RootPath`: no `..`, absolute paths, escaping symlinks or hardlinks to host
  files ([threat model T1, T2](../../security/threat-model.md#5-threats-and-mitigations)).
- Archives are streamed, never buffered whole; decompression is bounded (T15).
- A local directory context is never modified.

## Refs

- **Design:** [design/overlay-snapshot.md#state-directory-layout](../../design/overlay-snapshot.md#state-directory-layout), [design/cache-and-store.md#cache-keys](../../design/cache-and-store.md#cache-keys) (context file digests after `.dockerignore`)
- **Decisions:** [ADR-0008](../../adr/0008-out-of-scope-bake-warmer-cloud-contexts.md) (four sources, open trait), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (TLS trust store), [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) (trusting a context is the user's call)
- **Evidence:** none measured; [survey 03 §5](../../research/survey/03-rust-architecture.md#5-estimated-code-size)
  estimates ~500 lines; [exp-05](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) for TLS in `scratch`.
- **kaniko:** `kaniko@44aa7ddc0 pkg/buildcontext/buildcontext.go:40-72`
  (trait, prefix dispatch), `dir.go:25-27`, `tar.go:36-60`, `git.go:56-150`,
  `https.go:38-76`; `cmd/executor/cmd/root.go:595-633` (bare path,
  `--context-sub-path`); `pkg/util/fs_util.go:1195-1204` (ignore file lookup)
- **External:** [Docker docs, git URL fragments](https://docs.docker.com/build/concepts/context/#url-fragments); BuildKit v0.34.0 [dockerui/config.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerui/config.go#L415) (`<Dockerfile>.dockerignore`)

## Tests

- T0: `parse_spec` table against kaniko's prefixes; a directory context on a
  temp dir; ignore-file lookup order. Compare with kaniko's
  `integration_with_context_test.go` and `integration_with_stdin_test.go`.
- T0 hostile tars (`../x`, `/etc/x`, a symlink then a write through it, a
  hardlink to `/etc/passwd`, huge or endless entries): refused or confined.
- T0 HTTPS against a loopback server with a test CA; git against a local
  repository [open: Q-045].

## Metrics

- `rnk_context.load` (`files`, `dirs`, `bytes`, `files_ignored`);
  `rnk_context.ignore` when filtering is a pass of its own; Phase 4
  `rnk_context.hash` ([rows](../../design/metrics.md#rnk-context)).
- Count in the walk and record the totals once: no span or timer per file.

## Pitfalls

- kaniko takes a `--context` without `://` as a directory and dispatches
  `dir://`, `tar://`, `git://`, `https://` (`root.go:599-601`,
  `buildcontext.go:53-71`); `https://` on Azure Blob hosts goes to an Azure
  source (`:63-65`) ranikust lacks (ADR-0008) [open: Q-046].
- kaniko's `git://` clones over HTTPS (HTTP with `GIT_PULL_METHOD`), takes
  `GIT_USERNAME`/`GIT_PASSWORD`/`GIT_TOKEN` and reads `#ref#commit`
  (`git.go:56-128`, `:197-220`); Docker uses `#ref:dir`
  ([docs](https://docs.docker.com/build/concepts/context/#url-fragments)) [open: Q-045].
- `tar://stdin` is a gzip stream on stdin (`tar.go:41-56`);
  `--context-sub-path` applies after fetching (`root.go:625-630`).
- HTTPS needs the explicit trust store: reqwest's default fails in `scratch`
  ([exp-05](../../research/experiments/05-rust-ecosystem.md#q1-static-binary), ADR-0020).
- A static binary in `scratch` has no `git` to call [open: Q-045].
- `<Dockerfile>.dockerignore` wins over `<context>/.dockerignore` in kaniko
  (`fs_util.go:1195-1204`) and BuildKit; kaniko's integration set uses it.
- BuildKit's walk passes each directory's `MatchInfo` to its children and
  prunes with fsutil's rule: `rnk-ignore`'s `matches_using_parent` and
  `may_reinclude_below`; its results can differ from
  `matches_or_parent_matches`; the walk is what counts (ADR-0027).

## Phase 1

- Local directory contexts, the `ContextSource` trait, and locating and
  reading the ignore file ([survey 04 §2](../../research/survey/04-risks-and-roadmap.md#2-mvp-and-10-scope)).
- Deferred to phase 5: tar, git and HTTPS contexts (1.0 scope). `ADD <url>`
  and `ADD <git>` belong to `rnk-builder`.
- [open: Q-027] Async or sync trait. [open: Q-028] Where the TLS
  configuration shared with `rnk-registry` lives; whether `rnk-context`
  may use `rnk-layer` to decompress tar contexts (same tier in
  ARCHITECTURE).
