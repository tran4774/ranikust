# ranikust @ ed718fb: architecture for the security audit

**Product and principals.** ranikust is a daemonless OCI image builder in Rust (successor to kaniko). It reads a Dockerfile and a local build context, runs `RUN` as uid 0, and pushes the image or writes a Docker tar or OCI layout. It ships as one static musl binary for hosts or containers. It is pre-alpha with no release (SECURITY.md).

The operator is trusted: flags, `--state-dir`, output paths, `--secret` sources, `DOCKER_CONFIG`/`HOME`/`DOCKER_AUTH_CONFIG`, `SSL_CERT_*`. The following are lower-trust:
- the Dockerfile author;
- the build-context owner;
- the base-image publisher (layer tars, image config: Env, User, WorkingDir, Shell);
- any registry named by the Dockerfile (manifests, blobs, auth challenges, realms);
- the `RUN` process (root, or `USER`) and whatever it leaves in the root (symlinks, hard links, devices, FIFOs, xattrs, setuid bits, long-lived processes);
- other builds or local users that share one `--state-dir` (threat-model.md:40-47).

Protected resources (threat-model.md:28-36, assets A1-A7):
- the host filesystem outside the build roots, and the host `/` itself;
- registry credentials and `--secret` values;
- the state directory (blobs, extracted layers, cache mounts, journals);
- the integrity of pushed images;
- the host's process and mount namespaces.

Accepted risks in the threat model (section 6) and in ADRs are not findings unless the code differs from what they accept. Examples: ADR-0048 (container detection can be forged), T14 (cache mounts are shared across builds), the host network and cloud metadata being reachable from `RUN`, and kaniko mode on `/` having no boundary but the container.

**Comparable baseline.** kaniko runs `RUN` with no chroot or namespaces, deletes the filesystem with `RemoveAll` (crosses mounts), lets grandchildren survive `setsid`, and sends credentials over plain HTTP to insecure registries (docs/research/survey/01-kaniko-today.md:91-100). BuildKit is the oracle for observable behaviour: secret tmpfs, default isolation (ADR-0003, ADR-0049). ranikust claims to be stricter than kaniko on each of these points.

**Stack and deployment.** Rust 1.99.1, edition 2024, 15 crates, `panic=abort` in release. Main dependencies:
- rustix and libc (syscalls);
- oci-client 0.18 and reqwest 0.13 with rustls/ring (registry);
- tar (read side only), flate2 and bundled zstd (layers);
- rayon and tokio;
- clap (flags).

Modes (`crates/ranikust/src/main.rs:234-456`, `setup.rs:104,181,199,298`):
- **Overlay mode.** Root that may mount. Each `RUN` runs in a full sandbox (`Isolation::Full`): NEWNS, NEWPID, NEWUTS, NEWIPC (plus NEWNET for `--network=none`), `pivot_root` or the MS_MOVE+chroot fallback, nodev, Docker masked paths, 14 capabilities, `setsid`, no NEWUSER, no `no_new_privs`.
- **Kaniko mode with per-stage directories.** Root that can mount, off `/`, outside a container. Full sandbox. Directories `<state>/builds/<id>/stages/<n>/rootfs` with hand-over and release (ADR-0059, ADR-0061, ADR-0062).
- **Kaniko mode on a container's `/`.** Root that cannot mount and a container was detected (`crates/ranikust/src/container.rs:126-176`). `Isolation::Direct`: no namespaces; only capability trimming (`rnk-sandbox/src/caps.rs:139`), then `setsid` and a sweep of leftover processes (`sweep.rs`). An anonymous store (O_TMPFILE). The root is emptied between stages. It is refused when root may mount, when `--network=none` is set, or when `/proc/sys` is writable (`main.rs:462-504`).
- **Otherwise** the build is refused.

Offline build/test limits: target-controlled code is not executed in this run. There is no OS sandbox allowed on the host, and the VM and T2c lanes have network. Every check is source-only; runtime-decisive leads become needs_validation with VM (T2) or T2c plans.

**Entry surfaces and paths.**
1. **Registry bytes.** `rnk-registry` (`client.rs`, `token.rs`, `io.rs`) → `rnk-store` tmp entry → `rnk-layer/src/decoder.rs` → `rnk-rootfs/src/extract.rs` → the stage root. There are three extraction views: store/overlay, live (kaniko), and literal.
2. **Dockerfile.** `rnk-dockerfile` (parser, shell expansion) → `rnk-builder/src/stage.rs` (RUN argv/env, USER/WORKDIR, config) → `rnk-sandbox`.
3. **Context.** `rnk-context` walk plus `rnk-ignore` → `rnk-builder/src/sources.rs`, `glob.rs` → `rnk-rootfs/src/copy.rs`. The `.dockerignore` next to the Dockerfile is read through a host path (`ranikust/src/build.rs:397-399`).
4. **RUN results.**
   - Overlay mode: the upperdir is exported (`rnk-snapshot/src/export.rs`).
   - Kaniko mode: the root is walked and hashed with xxh3 (`kaniko/walk.rs`), with an ignore list from mountinfo and `--ignore-path` (`kaniko/ignore.rs`).
   - Both feed `rnk-layer` writer → push (`rnk-registry` push, `rnk-builder/src/output.rs`).
5. **State dir.** `ranikust/src/statedir.rs` checks it. `rnk-store/src/store.rs` handles get, publish, caches and cleanup. `rnk-sandbox/src/journal.rs` replays journals at start.
6. **Secrets and credentials.**
   - `--secret` is read by `rnk-builder/src/mounts.rs`: a tmpfs file in Full mode, env only in Direct mode.
   - Credentials come from `DOCKER_AUTH_CONFIG` or config.json (`rnk-registry/src/credentials.rs`). They are withheld over plain HTTP or unverified TLS by `client.rs:275-340`.
   - RUN stdout and stderr are relayed raw (`relay.rs`).
7. **CLI and environment.** `cli.rs` and `config.rs`. `RANIKUST_DEBUG_FAULT` is live in release builds (`main.rs:131`).
8. **Supply chain.** `.github/workflows/ci.yml`, `Cargo.lock`, `deny.toml`, `justfile`, and the test lanes `tests/vm` and `tests/container`.

**Trust boundaries and strongest source-visible control.**
- **Untrusted path to host filesystem:** `RootPath` with `openat2(RESOLVE_IN_ROOT|NO_MAGICLINKS)`, pinning with NO_SYMLINKS|NO_XDEV (`rnk-rootfs/src/root.rs`). Deletion goes by descriptor and never crosses a mount id (`delete.rs`).
- **RUN to host:** in Full mode, namespaces, pivot_root, nodev, masks and capability drop (`rnk-sandbox/src/child.rs:355-476`). In Direct mode, only the outer container.
- **Registry to credentials:** `withheld_because` and the HTTPS-realm rule (`rnk-registry/src/client.rs:275-340,1320-1335`).
- **Image bytes to store:** digest and size checks (`client.rs:527-611`) and the diff_id check (`rnk-snapshot/src/layer.rs:109-135`, `kaniko/root.rs:885-1020`).
- **Co-tenant to state dir:** owner and mode checks in `statedir.rs:53-97,324`; per-digest flock and atomic rename in `store.rs`. Published entries are not re-verified.
- **Host to kaniko-on-`/` deletion:** container detection plus the refusals in `main.rs:462-504`.

**Starting paths.** All of `crates/*/src`, `crates/*/tests` (to see what is not tested), `docs/security/threat-model.md`, `docs/adr/`, `.github/workflows/ci.yml`, `tests/vm`, `tests/container`.

**Prior coverage.** None; this is the first run. Nothing is carried or excluded.

**Companion selection.**
- MEMORY-SAFETY-AND-BINARY: root syscall wrappers, unsafe code after clone, privileged kernel interfaces, tar/PAX parsing.
- RESOURCE-EXHAUSTION-AND-AVAILABILITY: decompression, unbounded tar or manifest data, a shared state dir, cleanup after a crash.
- SUPPLY-CHAIN-AND-RELEASE: base images as build inputs, CI, dependencies, test lanes.
- DATA-ISOLATION-AND-LIFECYCLE: a store shared by builds, cache mounts, deletion and cleanup.
- CLOUD-AND-DEPLOYMENT: container and pod deployment, container detection, secrets in kaniko mode on `/`.
- DESKTOP-MOBILE-AND-LOCAL-IPC: a privileged local process acting on attacker-shaped files and a shared state dir.
- WEB-PROTOCOL-AND-AUTH: registry HTTP auth (Bearer/Basic realms, tokens, TLS).
- AI-AND-LLM (added after critic c4): the repository's agent automation in `.claude/` (permission allowlist, hook, subagents with Bash).
- Excluded:
  - CLIENT-SIDE: there is no browser.
  - PROTOCOLS-RPC-AND-MESSAGING: there are no RPC, broker or queue; the registry HTTP surface is covered by WEB-PROTOCOL-AND-AUTH.
