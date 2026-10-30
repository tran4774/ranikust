# rnk-context

> **Status:** in progress (Phase 1 done; tar, git, HTTPS are Phase 5) · **Phase:** 1 (local directory) ·
> **Depends on:** `rnk-rootfs`, `rnk-ignore` (ADR-0027, [open: Q-028]) · **Used by:** `rnk-builder` · **Path:** `crates/rnk-context/`

**TL;DR:** The build context from a local directory, tar, git or HTTPS, behind
an open trait, and its walk exactly as BuildKit's filtered walk. Every
context is hostile input: it is reached only through `rnk_rootfs::RootPath`,
and archive and clone contents reach disk the same way.

## Owns

- Providing the build context from a local directory, a tar archive, a git
  repository or an HTTPS URL.
- The open trait through which new context sources are added later.
- Locating the ignore file: `<Dockerfile>.dockerignore` before
  `<context>/.dockerignore`, as kaniko and BuildKit do.
- Walking the context as BuildKit's filtered walk does (ADR-0027): the
  entries a build may read, in order.

## Must not

- Fetch from cloud object stores (S3, GCS, Azure) for now (ADR-0008).
- Interpret the Dockerfile or decide which files an instruction copies
  (`rnk-builder`), including resolving `COPY` source globs (Q-080, ADR-0032);
  match ignore patterns itself (`rnk-ignore`).
- Reach a context path with `std::fs` on a joined host path (`rnk-rootfs`).

## API

Phase 1 (`[design]` unless marked):

```rust
pub enum ContextSpec { Dir(PathBuf), Tar(String), Git(String), Https(String) }  // kaniko prefixes; #[non_exhaustive]
pub fn parse_spec(arg: &str) -> Result<ContextSpec>;  // dir://, tar://, git://, https://; bare path = dir; gs://, s3:// refused
pub trait ContextSource {                             // [decided] open trait, ADR-0008
    fn open(&self) -> Result<Context>;                // sync for now [open: Q-027]
}
pub struct DirSource;                                 // DirSource::new(path): canonicalized, used in place
pub fn open(spec: &ContextSpec) -> Result<Context>;   // Dir only; Tar, Git, Https -> Error::Unsupported until Phase 5
impl Context {
    pub fn root(&self) -> &RootPath;                  // unfiltered: ignored files are reachable through it
    pub fn origin(&self) -> &Path;                    // the canonical directory, for messages
    pub fn sub_context(&self, sub: impl AsRef<Path>) -> Result<Context>;  // --context-sub-path; `..` clamped (Q-081, ADR-0032)
    pub fn find_dockerignore(&self, dockerfile: Option<&Path>) -> Result<Option<IgnoreFile>>;
    pub fn matcher(&self, ignore: &IgnoreFile) -> Result<rnk_ignore::Matcher>;
    pub fn walk<'c>(&'c self, ignore: Option<&'c Matcher>) -> Walk<'c>;  // Iterator<Item = Result<Entry>>
}
pub struct IgnoreFile { pub source: IgnoreSource, pub contents: Vec<u8> }
pub enum IgnoreSource { Dockerfile(PathBuf), ContextRoot }
pub struct Entry { pub path: PathBuf, pub meta: EntryMeta }  // relative, never empty; lstat-like meta
impl Walk<'_> { pub fn stats(&self) -> WalkStats }            // files, dirs, bytes, files_ignored
```

Planned for Phase 5: tar, git and HTTPS sources unpacked into
`builds/<build-id>/context/` (`BuildPaths` comes back as a source's
constructor argument, not `open`'s); `ContextSpec` keeps strings until then.

## Invariants

- Nothing from a tar, git or HTTPS context is written except through
  `RootPath`: no `..`, absolute paths, escaping symlinks or hardlinks to host
  files ([threat model T1, T2](../../security/threat-model.md#5-threats-and-mitigations)).
- Archives are streamed, never buffered whole; decompression is bounded (T15).
- A local directory context is never modified.
- The walk opens nothing but directories, each inside the root with
  `no_follow`; entries get `statx(AT_SYMLINK_NOFOLLOW)` on the directory's
  fd. Symlinks are reported, never followed; FIFOs and devices are
  reported, never opened.
- Walk order is a function of the tree and the patterns alone: byte-wise
  names per directory, depth first.

## Refs

- **Design:** [design/overlay-snapshot.md#state-directory-layout](../../design/overlay-snapshot.md#state-directory-layout), [design/cache-and-store.md#cache-keys](../../design/cache-and-store.md#cache-keys) (context file digests after `.dockerignore`)
- **Decisions:** [ADR-0008](../../adr/0008-out-of-scope-bake-warmer-cloud-contexts.md) (four sources, open trait), [ADR-0027](../../adr/0027-context-filtering-follows-buildkit-walk.md) (the walk), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (TLS trust store), [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) (trusting a context is the user's call)
- **Evidence:** none measured; [survey 03 §5](../../research/survey/03-rust-architecture.md#5-estimated-code-size)
  estimates ~500 lines; [exp-05](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) for TLS in `scratch`.
- **kaniko:** `kaniko@44aa7ddc0 pkg/buildcontext/buildcontext.go:40-72`
  (trait, prefix dispatch), `dir.go:25-27`, `tar.go:36-60`, `git.go:56-150`,
  `https.go:38-76`; `cmd/executor/cmd/root.go:595-633` (bare path,
  `--context-sub-path`); `pkg/util/fs_util.go:1195-1204` (ignore file lookup);
  `pkg/util/command_util.go:145-185` (`COPY` globs, Q-080)
- **External:** [Docker docs, git URL fragments](https://docs.docker.com/build/concepts/context/#url-fragments); BuildKit v0.34.0 [dockerui/config.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerui/config.go#L415) (`<Dockerfile>.dockerignore`); fsutil `83cac42c1c52` `filter.go` (`filterFS.Walk`)

## Tests

- Done (T0, temp dirs): `parse_spec` against kaniko's prefixes; every case
  of `rnk-ignore`'s walk golden (`tests/golden/walk.txt`) on a real tree
  (`tests/walk_golden.rs`); order, symlinks in and out, a FIFO, a non-UTF-8
  name, held-back directories, ignore-file precedence and symlink
  confinement, sub-contexts, an unreadable directory (`tests/context.rs`);
  span totals (`tests/metrics.rs`, alone in its binary because `tracing`
  caches callsite interest per process).
- To do: compare with kaniko's `integration_with_context_test.go` and
  `integration_with_stdin_test.go` (task 13).
- T0 hostile tars (`../x`, `/etc/x`, a symlink then a write through it, a
  hardlink to `/etc/passwd`, huge or endless entries): refused or confined.
- T0 HTTPS against a loopback server with a test CA; git against a local
  repository [open: Q-045].

## Metrics

- `rnk_context.load` (`files`, `dirs`, `bytes`, `files_ignored`) around the
  walk, totals recorded when the `Walk` is dropped; `rnk_context.ignore`
  (`patterns`) around compiling the ignore file, since filtering happens
  inside the walk; Phase 4 `rnk_context.hash`
  ([rows](../../design/metrics.md#rnk-context)).
- Count in the walk and record the totals once: no span or timer per file.

## Pitfalls

- kaniko takes a `--context` without `://` as a directory and dispatches
  `dir://`, `tar://`, `git://`, `https://` (`root.go:599-601`,
  `buildcontext.go:53-71`); `https://` on Azure Blob hosts goes to an Azure
  source (`:63-65`) ranikust lacks (ADR-0008) (Q-046, ADR-0032).
- kaniko's `git://` clones over HTTPS (HTTP with `GIT_PULL_METHOD`), takes
  `GIT_USERNAME`/`GIT_PASSWORD`/`GIT_TOKEN` and reads `#ref#commit`
  (`git.go:56-128`, `:197-220`); Docker uses `#ref:dir`
  ([docs](https://docs.docker.com/build/concepts/context/#url-fragments)) [open: Q-045].
- `tar://stdin` is a gzip stream on stdin (`tar.go:41-56`);
  `--context-sub-path` applies after fetching (`root.go:625-630`), and
  kaniko ignores it for a bare path, which returns before that line
  (`root.go:599-601`) (Q-081, ADR-0032).
- HTTPS needs the explicit trust store: reqwest's default fails in `scratch`
  ([exp-05](../../research/experiments/05-rust-ecosystem.md#q1-static-binary), ADR-0020).
- A static binary in `scratch` has no `git` to call [open: Q-045].
- `<Dockerfile>.dockerignore` wins over `<context>/.dockerignore` in kaniko
  (`fs_util.go:1195-1204`) and BuildKit; kaniko's integration set uses it.
  The file beside the Dockerfile may lie outside the context: it is opened
  with `O_NOFOLLOW` and must be a regular file, so a symlink there is an
  error rather than a way to read any host file.
- BuildKit's walk passes each directory's `MatchInfo` to its children and
  prunes with fsutil's rule: `rnk-ignore`'s `matches_using_parent` and
  `may_reinclude_below`; its results can differ from
  `matches_or_parent_matches`; the walk is what counts (ADR-0027).
- An excluded directory kept for a possible exception is reported only once
  something inside it is, outermost first, as fsutil does; a directory is
  reported before the error reading it, and the walk stops at that error.
- `Context::root()` sees ignored files too: whatever reads sources for
  `COPY` must go through the walk's filter, or ignored files leak into the
  image (Q-080, ADR-0032).

## Phase 1

- Done: local directory contexts, the `ContextSource` trait, locating and
  reading the ignore file, the BuildKit walk with its spans
  ([survey 04 §2](../../research/survey/04-risks-and-roadmap.md#2-mvp-and-10-scope)).
- Not here: resolving `COPY` sources (`rnk-builder`, task 11, Q-080); the
  content hash is Phase 4 (`rnk_context.hash`).
- Deferred to phase 5: tar, git and HTTPS contexts (1.0 scope). `ADD <url>`
  and `ADD <git>` belong to `rnk-builder`.
- [open: Q-027] Async or sync trait (sync for now). [open: Q-028] Where
  the TLS configuration shared with `rnk-registry` lives; whether
  `rnk-context` may use `rnk-layer` to decompress tar contexts (same tier
  in ARCHITECTURE); `rnk-context` → `rnk-ignore` is used in the interim.
