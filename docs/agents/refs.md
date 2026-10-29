# References

**TL;DR:** External specs, kernel docs, upstream code and the crates chosen
or considered, with links checked on 2026-10-30. Crate versions and
licenses are crates.io's latest at that date, not what `Cargo.lock` pins.

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
| moby `patternmatcher` (`.dockerignore`) | https://github.com/moby/patternmatcher |
| kaniko (cited fork; original, archived) | https://github.com/osscontainertools/kaniko, https://github.com/GoogleContainerTools/kaniko |
| go-containerregistry (kaniko's image library) | https://github.com/google/go-containerregistry |
| runc (`MS_MOVE` fallback), youki (sandbox reference) | https://github.com/opencontainers/runc, https://github.com/youki-dev/youki |
| `oci-client` source | https://github.com/oras-project/rust-oci-client |

## Crates

From survey 03 §3 and `exp-05`. Status: **chosen** (an ADR or the survey
picked it), **candidate** (`[open: Q-012]`, Phase 2 `[open: Q-038]`; see [open-questions.md](../open-questions.md#open)),
**rejected**.

| Crate | Version, license | Role | Status |
|---|---|---|---|
| [oci-client](https://crates.io/crates/oci-client) | 0.18.0, Apache-2.0 | Registry client, behind a wrapper | chosen |
| [oci-spec](https://crates.io/crates/oci-spec) | 0.10.0, Apache-2.0 | OCI types | chosen |
| [docker_credential](https://crates.io/crates/docker_credential) | 1.4.0, MIT OR Apache-2.0 | `config.json`, credential helpers | chosen; may be vendored |
| [tar](https://crates.io/crates/tar) | 0.4.46, MIT OR Apache-2.0 | Tar with hand-built headers; never `append_dir_all` | chosen |
| [flate2](https://crates.io/crates/flate2), [zlib-rs](https://crates.io/crates/zlib-rs) | 1.1.10, MIT OR Apache-2.0; 0.6.8, Zlib | Deflate under the pigz-style gzip | chosen |
| [zstd](https://crates.io/crates/zstd) | 0.14.3, BSD-3-Clause | Multithreaded zstd, fed 128 KiB chunks | chosen |
| [gzp](https://crates.io/crates/gzp) | 2.0.4, Unlicense/MIT | Parallel gzip | candidate |
| [rustls](https://crates.io/crates/rustls) | 0.23.46, Apache-2.0 OR ISC OR MIT | TLS, no OpenSSL (`ADR-0020`, `ADR-0024`) | chosen |
| [ring](https://crates.io/crates/ring) | 0.17.14, Apache-2.0 AND ISC | rustls crypto provider; 5.1 MB static binary against 7.3 MB (`exp-05`) | chosen (`ADR-0024`) |
| [aws-lc-rs](https://crates.io/crates/aws-lc-rs) | 1.19.0, ISC AND (Apache-2.0 OR ISC) | The other provider; reqwest 0.13's default, kept out of TLS | rejected (`ADR-0024`) |
| [webpki-roots](https://crates.io/crates/webpki-roots), [webpki-root-certs](https://crates.io/crates/webpki-root-certs) | 1.0.11, CDLA-Permissive-2.0 | Embedded roots, trusted together with the host's CAs; `webpki-root-certs` gives the DER certificates `oci-client` takes | chosen (`ADR-0024`) |
| [rustls-native-certs](https://crates.io/crates/rustls-native-certs) | 0.8.5, Apache-2.0 OR ISC OR MIT | Reads the host's CAs (`/etc/ssl/certs`, `SSL_CERT_FILE`, `SSL_CERT_DIR`), as `exp-05` did | candidate (`ADR-0024`) |
| [sha2](https://crates.io/crates/sha2) | 0.11.1, MIT OR Apache-2.0 | sha256 (or `aws-lc-rs`) | candidate |
| [reqwest](https://crates.io/crates/reqwest), [hyper](https://crates.io/crates/hyper) | 0.13.5, MIT OR Apache-2.0; 1.12.0, MIT | HTTP | candidate |
| [tokio](https://crates.io/crates/tokio) | 1.54.1, MIT | Async runtime | chosen |
| [rayon](https://crates.io/crates/rayon) | 1.12.0, MIT OR Apache-2.0 | CPU parallelism: hashing, compression, walk | chosen |
| [rustix](https://crates.io/crates/rustix), [nix](https://crates.io/crates/nix) | 1.1.6, Apache-2.0 (LLVM) OR Apache-2.0 OR MIT; 0.31.3, MIT | Syscalls; `rustix` preferred ([rules.md](rules.md#code-traps)) | candidate |
| [mimalloc](https://crates.io/crates/mimalloc) | 0.1.52, MIT | Global allocator on musl, required | chosen (`ADR-0020`) |
| [tracing](https://crates.io/crates/tracing), [tracing-subscriber](https://crates.io/crates/tracing-subscriber) | 0.1.44, 0.3.23, MIT | Spans and logs in every crate; the subscriber, per-layer filters and the metrics layer in `rnk-metrics` ([docs](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/layer/)); `fmt` and `registry` features only | chosen (`ADR-0025`) |
| [opentelemetry-otlp](https://crates.io/crates/opentelemetry-otlp) | 0.33.1, Apache-2.0 | Trace and metric export | candidate (`[open: Q-058]`) |
| [serde_json](https://crates.io/crates/serde_json) | 1.0.154, MIT OR Apache-2.0 | JSON: the metrics report (`rnk-metrics`) | chosen |
| [libc](https://crates.io/crates/libc) | 0.2.191, MIT OR Apache-2.0 | `getrusage` in `rnk-metrics`, which `rustix` 1.1 lacks | chosen |
| [clap](https://crates.io/crates/clap) | 4.6.7, MIT OR Apache-2.0 | CLI parsing (named in survey 03) | candidate |
| [highway](https://crates.io/crates/highway), [xxhash-rust](https://crates.io/crates/xxhash-rust), [jwalk](https://crates.io/crates/jwalk) | 1.3.0, MIT; 0.8.19, BSL-1.0; 0.9.0, MIT | Snapshot hash, parallel walk (Phase 2) | candidate |
| [parse-dockerfile](https://crates.io/crates/parse-dockerfile), [ignore](https://crates.io/crates/ignore) | 0.1.8; 0.4.34 | Dockerfile parsing, `.dockerignore` | rejected (`ADR-0015`, `exp-04`) |
| [libcontainer](https://crates.io/crates/libcontainer) | 0.7.0, Apache-2.0 | youki's runtime | reference only |

## Tools and conventions

| What | Link |
|---|---|
| just, cargo-deny, mdBook, git-cliff, cargo-zigbuild | https://github.com/casey/just, https://embarkstudios.github.io/cargo-deny/, https://rust-lang.github.io/mdBook/, https://git-cliff.org/, https://github.com/rust-cross/cargo-zigbuild |
| MADR, Conventional Commits, Keep a Changelog | https://adr.github.io/madr/, https://www.conventionalcommits.org/en/v1.0.0/, https://keepachangelog.com/en/1.1.0/ |
| AGENTS.md convention | https://agents.md/ |

## Claude Code

| What | Link |
|---|---|
| Overview | https://code.claude.com/docs/en/overview |
| Skills; subagents; slash commands | https://code.claude.com/docs/en/skills, https://code.claude.com/docs/en/sub-agents, https://code.claude.com/docs/en/slash-commands |
| Hooks; settings; permissions | https://code.claude.com/docs/en/hooks, https://code.claude.com/docs/en/settings, https://code.claude.com/docs/en/iam |
| Memory (`CLAUDE.md`); common workflows (plan mode) | https://code.claude.com/docs/en/memory, https://code.claude.com/docs/en/common-workflows |
