# References

**TL;DR:** External specs, kernel docs, upstream code and the crates chosen
or considered. Links last checked on 2026-10-30; licences are crates.io's at
that date.

## Specs

| What | Link |
|---|---|
| OCI image spec; layers and whiteouts; image layout; image index; config (`diff_ids`) | https://github.com/opencontainers/image-spec/blob/main/spec.md, [layer.md](https://github.com/opencontainers/image-spec/blob/main/layer.md), [image-layout.md](https://github.com/opencontainers/image-spec/blob/main/image-layout.md), [image-index.md](https://github.com/opencontainers/image-spec/blob/main/image-index.md), [config.md](https://github.com/opencontainers/image-spec/blob/main/config.md) |
| OCI distribution spec (pull, push, blob mount) | https://github.com/opencontainers/distribution-spec/blob/main/spec.md |
| OCI runtime spec | https://github.com/opencontainers/runtime-spec/blob/main/spec.md |
| Docker image manifest v2, schema 2 | https://distribution.github.io/distribution/spec/manifest-v2-2/ |
| Docker image spec (config, `docker save` layout) | https://github.com/moby/docker-image-spec |
| Dockerfile reference | https://docs.docker.com/reference/dockerfile/ |
| Build context and `.dockerignore` | https://docs.docker.com/build/concepts/context/ |

## Linux

| What | Link |
|---|---|
| overlayfs | https://docs.kernel.org/filesystems/overlayfs.html |
| User namespaces; `openat2` (`RESOLVE_IN_ROOT`); `pivot_root` | https://man7.org/linux/man-pages/man7/user_namespaces.7.html, https://man7.org/linux/man-pages/man2/openat2.2.html, https://man7.org/linux/man-pages/man2/pivot_root.2.html |
| Capabilities, bounding set | https://man7.org/linux/man-pages/man7/capabilities.7.html |
| seccomp | https://man7.org/linux/man-pages/man2/seccomp.2.html, https://docs.kernel.org/userspace-api/seccomp_filter.html |
| Mount namespaces; `mount`; `unshare` | https://man7.org/linux/man-pages/man7/mount_namespaces.7.html, https://man7.org/linux/man-pages/man2/mount.2.html, https://man7.org/linux/man-pages/man2/unshare.2.html |
| New mount API: `fsopen`, `fsconfig` | https://man7.org/linux/man-pages/man2/fsopen.2.html, https://man7.org/linux/man-pages/man2/fsconfig.2.html |
| `statx` (`STATX_MNT_ID`); `PR_SET_PDEATHSIG`; `/proc/<pid>/mountinfo` | https://man7.org/linux/man-pages/man2/statx.2.html, https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html, https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html |
| cgroup v2 | https://docs.kernel.org/admin-guide/cgroup-v2.html |
| Docker defaults: seccomp profile, capabilities | https://github.com/moby/profiles/blob/main/seccomp/default.json, https://github.com/moby/moby/blob/master/daemon/pkg/oci/caps/defaults.go |

## Upstream code

| What | Link |
|---|---|
| BuildKit Dockerfile parser, instructions, shell lexer (ported by hand, `ADR-0015`) | https://github.com/moby/buildkit/tree/master/frontend/dockerfile/parser, [instructions](https://github.com/moby/buildkit/tree/master/frontend/dockerfile/instructions), [shell/lex.go](https://github.com/moby/buildkit/blob/master/frontend/dockerfile/shell/lex.go) |
| BuildKit upperdir differ (`WriteUpperdir`), `RUN` stubs | https://github.com/moby/buildkit/blob/master/util/overlay/overlay_linux.go, https://github.com/moby/buildkit/blob/master/executor/stubs.go |
| containerd overlay snapshotter | https://github.com/containerd/containerd/tree/main/plugins/snapshots/overlay |
| moby `patternmatcher` (`.dockerignore`, ported in `rnk-ignore`) | https://github.com/moby/patternmatcher |
| fsutil filtered walk (BuildKit's context transfer, `.dockerignore` pruning) | https://github.com/tonistiigi/fsutil/blob/master/filter.go |
| kaniko (cited fork; original, archived) | https://github.com/osscontainertools/kaniko, https://github.com/GoogleContainerTools/kaniko |
| go-containerregistry (kaniko's image library) | https://github.com/google/go-containerregistry |
| runc (`MS_MOVE` fallback), youki (sandbox reference) | https://github.com/opencontainers/runc, https://github.com/youki-dev/youki |
| `oci-client` source | https://github.com/oras-project/rust-oci-client |

## Crates

From survey 03 section 3 and `exp-05`. Versions are in `Cargo.lock`; licences
are checked by `deny.toml`. Status: **chosen** (an ADR or the survey picked
it), **candidate** (later phases; see the
[`question` issues](https://github.com/tran4774/ranikust/issues?q=is%3Aissue+label%3Aquestion)),
**rejected**.

| Crate | Licence | Role | Status |
|---|---|---|---|
| [oci-client](https://crates.io/crates/oci-client) | Apache-2.0 | Registry client, behind a wrapper | chosen |
| [oci-spec](https://crates.io/crates/oci-spec) | Apache-2.0 | OCI types of `oci-client`; `rnk-oci` owns its own (0.10 serializes from hash maps, whose order changes the digest, and has no Docker-only config fields) | `oci-client` only |
| [serde](https://crates.io/crates/serde), [serde_json](https://crates.io/crates/serde_json) | MIT OR Apache-2.0 | (De)serialization of image types, canonical JSON, the metrics report | chosen (`rnk-oci`, `rnk-metrics`) |
| [thiserror](https://crates.io/crates/thiserror) | MIT OR Apache-2.0 | `Error` enums of the library crates; no `anyhow` in libraries | chosen |
| [proptest](https://crates.io/crates/proptest) | MIT OR Apache-2.0 | Property tests (dev only), `std` feature only, no failure files | chosen |
| [tempfile](https://crates.io/crates/tempfile) | MIT OR Apache-2.0 | Temp dirs for T0 tests, removed on drop | chosen (tests) |
| [docker_credential](https://crates.io/crates/docker_credential) | MIT OR Apache-2.0 | `config.json`, credential helpers | candidate for the Phase 5 helpers; `rnk-registry` reads `config.json` itself |
| [tar](https://crates.io/crates/tar) | MIT OR Apache-2.0 | Reading layers in `rnk-rootfs` (`entries()` only, never `unpack`) and in `rnk-layer`'s tests; `rnk-layer` builds its PAX headers itself (`crates/rnk-layer/src/header.rs`); never `append_dir_all` | chosen |
| [flate2](https://crates.io/crates/flate2), [zlib-rs](https://crates.io/crates/zlib-rs) | MIT OR Apache-2.0; Zlib | Deflate under the pigz-style gzip | chosen |
| [zstd](https://crates.io/crates/zstd) | BSD-3-Clause | zstd layers as independent 1 MiB frames compressed on `rnk-layer`'s workers (never libzstd's multithreading), and the decoder; no default features | chosen (`rnk-layer`) |
| [gzp](https://crates.io/crates/gzp) | Unlicense/MIT | Parallel gzip | rejected: not deterministic across thread counts (`exp-05`) |
| [rustls](https://crates.io/crates/rustls) | Apache-2.0 OR ISC OR MIT | TLS, no OpenSSL (`ADR-0020`, `ADR-0024`) | chosen |
| [ring](https://crates.io/crates/ring) | Apache-2.0 AND ISC | rustls crypto provider; 5.1 MB static binary against 7.3 MB (`exp-05`) | chosen (`ADR-0024`) |
| [aws-lc-rs](https://crates.io/crates/aws-lc-rs) | ISC AND (Apache-2.0 OR ISC) | The other provider; reqwest 0.13's default, kept out of TLS | rejected (`ADR-0024`) |
| [webpki-roots](https://crates.io/crates/webpki-roots), [webpki-root-certs](https://crates.io/crates/webpki-root-certs) | CDLA-Permissive-2.0 | Embedded roots, trusted together with the host's CAs; `webpki-root-certs` gives the DER certificates `oci-client` takes | chosen (`ADR-0024`) |
| [rustls-native-certs](https://crates.io/crates/rustls-native-certs) | Apache-2.0 OR ISC OR MIT | Reads the host's CAs (`/etc/ssl/certs`, `SSL_CERT_FILE`, `SSL_CERT_DIR`), as `exp-05` did | chosen (`ADR-0024`); used by `rnk-registry` |
| [rustls-pki-types](https://crates.io/crates/rustls-pki-types) | MIT OR Apache-2.0 | DER certificate type; PEM parsing of `--registry-certificate` files | chosen (`rnk-registry`) |
| [sha2](https://crates.io/crates/sha2) | MIT OR Apache-2.0 | sha256 in `rnk-layer`; `ring::digest` not yet measured against it (`[open: #29]`) | chosen |
| [reqwest](https://crates.io/crates/reqwest), [hyper](https://crates.io/crates/hyper) | MIT OR Apache-2.0; MIT | HTTP | `reqwest` chosen: the one `oci-client` uses, `rustls-no-provider`, used by `rnk-registry` for the `/v2/` probe and the identity-token exchange; `hyper` only for its tests' fake registry |
| [http-auth](https://crates.io/crates/http-auth) | MIT OR Apache-2.0 | `WWW-Authenticate` parsing, the parser `oci-client` uses | chosen (`rnk-registry`) |
| [base64](https://crates.io/crates/base64) | MIT OR Apache-2.0 | The `auth` field of `config.json` | chosen (`rnk-registry`) |
| [bytes](https://crates.io/crates/bytes), [futures-util](https://crates.io/crates/futures-util), [tokio-util](https://crates.io/crates/tokio-util) | MIT; MIT OR Apache-2.0; MIT | Push bodies as the byte streams `oci-client` takes | chosen (`rnk-registry`) |
| [http-body-util](https://crates.io/crates/http-body-util), [hyper-util](https://crates.io/crates/hyper-util) | MIT | The fake registry in `rnk-registry`'s tests | chosen (tests) |
| [tokio](https://crates.io/crates/tokio) | MIT | Async runtime | chosen |
| [rayon](https://crates.io/crates/rayon) | MIT OR Apache-2.0 | CPU parallelism: hashing, compression, the kaniko-mode walk (one task per directory over `getdents64`, `--snapshot-workers`, default 8, `ADR-0039`) | chosen |
| [rustix](https://crates.io/crates/rustix), [nix](https://crates.io/crates/nix) | Apache-2.0 (LLVM) OR Apache-2.0 OR MIT; MIT | Syscalls; `rustix` preferred ([rules.md](rules.md#code-traps)) | `rustix` chosen, `nix` rejected |
| [mimalloc](https://crates.io/crates/mimalloc) | MIT | Global allocator on musl, required; set in `crates/ranikust/src/main.rs`, no features | chosen (`ADR-0020`) |
| [tracing](https://crates.io/crates/tracing), [tracing-subscriber](https://crates.io/crates/tracing-subscriber) | MIT | Spans and logs in every crate; the subscriber, per-layer filters and the metrics layer in `rnk-metrics` ([docs](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/layer/)); `fmt` and `registry` features only | chosen (`ADR-0025`) |
| [opentelemetry-otlp](https://crates.io/crates/opentelemetry-otlp) | Apache-2.0 | Trace and metric export | candidate (`[open: #21]`) |
| [libc](https://crates.io/crates/libc) | MIT OR Apache-2.0 | `getrusage` in `rnk-metrics`, which `rustix` lacks | chosen |
| [clap](https://crates.io/crates/clap) | MIT OR Apache-2.0 | CLI parsing; derive, no colour | chosen for `ranikust` |
| [xxhash-rust](https://crates.io/crates/xxhash-rust) | BSL-1.0 | xxh3-128 content and xattr hash of the kaniko-mode walk; feature `xxh3` only | chosen (`ADR-0039`) |
| [regex](https://crates.io/crates/regex) | MIT OR Apache-2.0 | `rnk-ignore`'s compiled patterns (Go regexps translated; no Unicode tables) | chosen |
| [parse-dockerfile](https://crates.io/crates/parse-dockerfile), [ignore](https://crates.io/crates/ignore) | not recorded | Dockerfile parsing, `.dockerignore` | rejected (`ADR-0015`, `exp-04`) |
| [libcontainer](https://crates.io/crates/libcontainer) | Apache-2.0 | youki's runtime | reference only |

## Tools and conventions

| What | Link |
|---|---|
| just, cargo-deny, mdBook, git-cliff, cargo-zigbuild | https://github.com/casey/just, https://embarkstudios.github.io/cargo-deny/, https://rust-lang.github.io/mdBook/, https://git-cliff.org/, https://github.com/rust-cross/cargo-zigbuild |
| zig 0.16.0 (C compiler of the musl builds, through cargo-zigbuild 0.24.0) and its CI action | https://ziglang.org/download/, https://github.com/mlugg/setup-zig |
| MADR, Conventional Commits, Keep a Changelog | https://adr.github.io/madr/, https://www.conventionalcommits.org/en/v1.0.0/, https://keepachangelog.com/en/1.1.0/ |
| AGENTS.md convention | https://agents.md/ |

## Claude Code

| What | Link |
|---|---|
| Overview | https://code.claude.com/docs/en/overview |
| Skills; subagents; slash commands | https://code.claude.com/docs/en/skills, https://code.claude.com/docs/en/sub-agents, https://code.claude.com/docs/en/slash-commands |
| Hooks; settings; permissions | https://code.claude.com/docs/en/hooks, https://code.claude.com/docs/en/settings, https://code.claude.com/docs/en/iam |
| Memory (`CLAUDE.md`); common workflows (plan mode) | https://code.claude.com/docs/en/memory, https://code.claude.com/docs/en/common-workflows |
