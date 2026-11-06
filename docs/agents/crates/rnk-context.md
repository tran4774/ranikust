# rnk-context

> **Status:** in progress · **Phase:** 1 (tar, git, HTTPS: 5) ·
> **Depends on:** `rnk-rootfs`, `rnk-ignore` (ADR-0027, ADR-0115) ·
> **Used by:** `rnk-builder`, `ranikust` · **Path:** `crates/rnk-context/`

**TL;DR:** The build context from a local directory (tar, git and HTTPS are
parsed and refused until Phase 5), behind an open trait, and its walk exactly
as BuildKit's filtered walk. Every context is hostile input: it is reached
only through `rnk_rootfs::RootPath`, and archive and clone contents reach
disk the same way.

## Owns

- Providing the build context from a local directory (built), a tar archive,
  a git repository or an HTTPS URL (Phase 5).
- The open trait through which new context sources are added.
- Locating the ignore file: `<Dockerfile>.dockerignore` before
  `<context>/.dockerignore`, as kaniko and BuildKit do.
- Walking the context as BuildKit's filtered walk does (ADR-0027): the
  entries a build may read, in order.

## Must not

Boundaries: `crates/rnk-context/src/lib.rs`. In short:

- Fetch from cloud object stores (S3, GCS, Azure) for now (ADR-0008).
- Interpret the Dockerfile or decide which files an instruction copies
  (`rnk-builder`, including resolving `COPY` source globs, ADR-0032); match
  ignore patterns itself (`rnk-ignore`).
- Reach a context path with `std::fs` on a joined host path (`rnk-rootfs`).

## API

```rust
pub enum ContextSpec { Dir(PathBuf), Tar(String), Git(String), Https(String) }  // kaniko prefixes; #[non_exhaustive]
pub fn parse_spec(arg: &str) -> Result<ContextSpec>;  // tar://, git://, https://; bare path = dir; gs://, s3:// refused; `dir://` refused with Error::DirScheme, naming the plain path (ADR-0053, ADR-0054)
pub trait ContextSource {                             // [decided] open trait, ADR-0008
    fn open(&self) -> Result<Context>;                // sync, ADR-0115
}
pub struct DirSource;                                 // DirSource::new(path): opened with RESOLVE_NO_SYMLINKS, a symlink anywhere refused (Error::Symlink, ADR-0076), used in place
pub fn open(spec: &ContextSpec) -> Result<Context>;   // Dir only; Tar, Git, Https -> Error::Unsupported until Phase 5
impl Context {
    pub fn root(&self) -> &RootPath;                  // unfiltered: ignored files are reachable through it
    pub fn origin(&self) -> &Path;                    // the directory (no symlink in it), for messages
    pub fn fence(&self) -> &Fence;                    // the context's directory, which no output may lead into (ADR-0076)
    pub fn owner(&self) -> u32;                       // the context directory's owner: a principal of its own (ADR-0076)
    pub fn open_file(&self, rel) -> rnk_rootfs::Result<OwnedFd>; // a regular file, symlinks followed inside only; the context rule of open_context_file (ADR-0076)
    pub fn sub_context(&self, sub: impl AsRef<Path>) -> Result<Context>;  // --context-sub-path, fetched contexts only; `..` clamped (ADR-0032, ADR-0053); keeps the fence
    pub fn find_dockerignore(&self, dockerfile: Option<DockerfileAt>) -> Result<Option<IgnoreFile>>; // DockerfileAt::Host(&HostFile): the file beside it, from the pinned directory, pinned and type-checked first (a symlink, FIFO or device refused unopened); DockerfileAt::Context(rel): read through the context
    pub fn matcher(&self, ignore: &IgnoreFile) -> Result<rnk_ignore::Matcher>;
    pub fn walk<'c>(&'c self, ignore: Option<&'c Matcher>) -> Walk<'c>;  // Iterator<Item = Result<Entry>>
}
pub struct IgnoreFile { pub source: IgnoreSource, pub contents: Vec<u8> }
pub enum IgnoreSource { Dockerfile(PathBuf), ContextRoot }
pub struct Entry { pub path: PathBuf, pub meta: EntryMeta }  // relative, never empty; lstat-like meta
impl Walk<'_> { pub fn stats(&self) -> WalkStats }            // files, dirs, bytes, files_ignored
```

For Phase 5, tar, git and HTTPS sources unpack into
`builds/<build-id>/context/`; `BuildPaths` then comes back as a source's
constructor argument, not `open`'s.

## Invariants

- Nothing from a tar, git or HTTPS context is written except through
  `RootPath`: no `..`, absolute paths, escaping symlinks or hardlinks to host
  files ([threat model T1, T2](../../security/threat-model.md#5-threats-and-mitigations)).
- Archives are streamed, never buffered whole; decompression is bounded (T15).
- A local directory context is never modified.
- The walk opens nothing but directories, each inside the root with
  `no_follow`; entries get `statx(AT_SYMLINK_NOFOLLOW)` on the directory's
  fd. Symlinks are reported, never followed; FIFOs and devices are reported,
  never opened.
- Walk order is a function of the tree and the patterns alone: byte-wise
  names per directory, depth first.

## Refs

- **Design:** [design/overlay-snapshot.md#state-directory-layout](../../design/overlay-snapshot.md#state-directory-layout), [design/cache-and-store.md#cache-keys](../../design/cache-and-store.md#cache-keys) (context file digests after `.dockerignore`)
- **Decisions:** [ADR-0008](../../adr/0008-out-of-scope-bake-warmer-cloud-contexts.md) (four sources, open trait), [ADR-0027](../../adr/0027-context-filtering-follows-buildkit-walk.md) (the walk), [ADR-0035](../../adr/0035-git-contexts-built-in-or-external-git.md) (git contexts), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (TLS trust store), [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) (trusting a context is the user's call), [ADR-0076](../../adr/0076-the-build-context-owner-is-a-lower-trust-principal.md) (the context's owner)
- **Open:** [#6](https://github.com/tran4774/ranikust/issues/6) (sync or async trait), [#7](https://github.com/tran4774/ranikust/issues/7) (planned crate dependencies), [#15](https://github.com/tran4774/ranikust/issues/15), [#16](https://github.com/tran4774/ranikust/issues/16), [#17](https://github.com/tran4774/ranikust/issues/17) (git, below)
- **Evidence:** none measured; [survey 03 §5](../../research/survey/03-rust-architecture.md#5-estimated-code-size) estimates ~500 lines; [exp-05](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) for TLS in `scratch`
- **kaniko:** `kaniko@44aa7ddc0 pkg/buildcontext/buildcontext.go:40-72` (trait, prefix dispatch), `dir.go:25-27`, `tar.go:36-60`, `git.go:56-150`, `https.go:38-76`; `cmd/executor/cmd/root.go:595-633` (bare path, `--context-sub-path`); `pkg/util/fs_util.go:1195-1204` (ignore file lookup); `pkg/util/command_util.go:145-185` (`COPY` globs)
- **External:** [Docker docs, git URL fragments](https://docs.docker.com/build/concepts/context/#url-fragments); BuildKit v0.34.0 [dockerui/config.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerui/config.go#L415) (`<Dockerfile>.dockerignore`); fsutil `83cac42c1c52` `filter.go` (`filterFS.Walk`)

## Tests

All T0 (temp dirs).

- `parse_spec` against kaniko's prefixes; every case of `rnk-ignore`'s walk
  golden (`tests/golden/walk.txt` there) on a real tree
  (`tests/walk_golden.rs`); order, symlinks in and out, a FIFO, a non-UTF-8
  name, held-back directories, ignore-file precedence and symlink
  confinement, sub-contexts, an unreadable directory (`tests/context.rs`);
  red team (ADR-0076): `--context` through a magic link (`/proc/self/cwd`,
  `/proc/thread-self/cwd`, `/proc/self/root/...`, `/proc/self/fd/<n>`,
  `/dev/fd/<n>`) is refused as a symlink
  (`red_team_a_context_through_a_magic_link_is_refused`); span totals
  (`tests/metrics.rs`, alone in its binary because `tracing` caches callsite
  interest per process).
- To do: compare with kaniko's `integration_with_context_test.go` and
  `integration_with_stdin_test.go`.
- Planned with Phase 5: hostile tars (`../x`, `/etc/x`, a symlink then a
  write through it, a hardlink to `/etc/passwd`, huge or endless entries)
  refused or confined; HTTPS against a loopback server with a test CA; git
  against a local repository, with both backends and both fragment forms
  (ADR-0035); the external `git` with a hostile repository (hooks,
  `core.fsmonitor`, `file://` submodules, a ref or URL starting with `-`)
  runs nothing.

## Metrics

- `rnk_context.load` (`files`, `dirs`, `bytes`, `files_ignored`) around the
  walk, totals recorded when the `Walk` is dropped; `rnk_context.ignore`
  (`patterns`) around compiling the ignore file, since filtering happens
  inside the walk; Phase 4 `rnk_context.hash`
  ([rows](../../design/metrics.md#rnk-context)).
- Count in the walk and record the totals once: no span or timer per file.

## Pitfalls

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
- `Context::root()` sees ignored files too: whatever reads sources for `COPY`
  must go through the walk's filter, or ignored files leak into the image
  (ADR-0032).
- kaniko takes a `--context` without `://` as a directory and dispatches
  `dir://`, `tar://`, `git://`, `https://` (`root.go:599-601`,
  `buildcontext.go:53-71`); `https://` on Azure Blob hosts goes to an Azure
  source (`:63-65`) ranikust lacks (ADR-0008, ADR-0032).
- `--context-sub-path` applies after fetching (`root.go:625-630`); kaniko
  ignores it for a bare path, which returns before that line
  (`root.go:599-601`). ranikust drops `dir://` and applies the sub-path to
  fetched contexts only, and refuses it with a local context (ADR-0032,
  ADR-0053, ADR-0054).
- HTTPS needs the explicit trust store: reqwest's default fails in `scratch`
  ([exp-05](../../research/experiments/05-rust-ecosystem.md#q1-static-binary), ADR-0020).

<a name="phase-1"></a>
## Phase 5: tar, git and HTTPS contexts

Not built: `open` refuses them. `ADD <url>` and `ADD <git>` belong to
`rnk-builder`. What is decided or known:

- `tar://stdin` is a gzip stream on stdin (`tar.go:41-56`).
- kaniko's `git://` clones over HTTPS (HTTP with `GIT_PULL_METHOD`), takes
  `GIT_USERNAME`/`GIT_PASSWORD`/`GIT_TOKEN` and reads `#ref#commit`
  (`git.go:56-128`, `:197-220`); Docker uses `#ref:dir`; both forms are
  accepted (ADR-0035).
- Plan (ADR-0035): a `GitSource` with two backends behind one trait: a
  built-in `gix` by default (a static binary in `scratch` has no `git` to
  call; TLS roots shared with `rnk-registry`, ADR-0024) and
  `--git-backend=external`, a `git` binary run by the ADR's hardened recipe
  (isolated from the host's configuration, hooks and `fsmonitor` off),
  configured by `--git-binary`, `--git-config`, `--git-arg` and
  `--git-host-config`. Refs, commits and URLs from the context argument are
  validated and passed after `--`. The clone lands in the store's temporary
  directory and is read through `RootPath`; `.git` is removed unless
  `BUILDKIT_CONTEXT_KEEP_GIT_DIR=1`; kaniko's `--git` keys and `GIT_*`
  variables work for both.
- Open: [#15](https://github.com/tran4774/ranikust/issues/15) fallback
  between the backends; [#16](https://github.com/tran4774/ranikust/issues/16)
  kaniko's commit with a subdirectory;
  [#17](https://github.com/tran4774/ranikust/issues/17) the built-in's
  `--git-config` keys; [#7](https://github.com/tran4774/ranikust/issues/7)
  where the TLS configuration shared with `rnk-registry` lives, and whether
  `rnk-context` may use `rnk-layer` to decompress tar contexts.
