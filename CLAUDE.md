# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

ranikust is a daemonless container image builder in Rust, a successor to
kaniko: it reads a Dockerfile, runs it, and pushes an OCI image from one
static musl binary that works inside a container or directly on a host.
Pre-alpha: Phase 1 (MVP) is in progress on branch `phase-1`; most crates are
still documented skeletons.

**For anything beyond this page, start at
[`docs/agents/README.md`](docs/agents/README.md):** it routes each kind of task
to the few short files worth reading (crate cards, rules, Phase 1 plan,
kaniko map, glossary, refs). Load long documents only when a card points to
them.

## Commands

```sh
just check                        # fmt check + clippy -D warnings + tests; run before calling work done
cargo test -p rnk-rootfs          # tests of one crate
cargo test -p rnk-rootfs <name>   # a single test (name filter)
just test <filter>                # name filter across the whole workspace
just ci                           # exactly what CI runs (also static musl build, aarch64 check, deny, docs)
just fmt | just lint | just deny | just doc | just book
```

The toolchain is pinned in `rust-toolchain.toml` (1.99.1, edition 2024);
`rustup` installs it on first use.

## Big picture

- **Two snapshot modes, chosen by probing at startup.** Overlay mode (may
  mount: root, privileged, rootless user namespaces) captures each
  instruction as an overlay upperdir and builds stages in parallel. kaniko
  mode (root but may not mount: default Docker, most pods) walks and hashes
  the rootfs like kaniko, sequentially by default. Elsewhere the tool
  refuses with guidance. See `ADR-0002`, `ADR-0004`, `ADR-0014`.
- **Build pipeline across crates:** `rnk-dockerfile` (+ `rnk-ignore`,
  `rnk-context`) → `rnk-sched` runs the stage DAG → `rnk-builder` executes
  instructions, using `rnk-snapshot` for stage roots and diffs, `rnk-sandbox`
  for `RUN`, `rnk-rootfs` for every filesystem access → `rnk-layer` encodes
  layers lazily → `rnk-oci`/`rnk-registry` assemble and push; `rnk-cache`
  keys and reuses layers; `rnk-store` holds blobs, extracted layers and
  cache mounts under `--state-dir`. `crates/ranikust` is the CLI only: it
  runs the privilege probes and picks the mode (`--mode`).
- Each crate's `lib.rs` has a `# Boundaries` section; it is authoritative
  for what the crate must not do. `ARCHITECTURE.md` is the full map.
- Why things are the way they are: `docs/adr/` (decisions),
  `docs/design/` (current design), `docs/research/` (frozen survey and
  experiments with the measurements).

## Must-follow rules

Full list in [`docs/agents/rules.md`](docs/agents/rules.md); the ones that can
damage the machine or the design:

- Never run kaniko mode with rootfs `/` on the host, never use `sudo`,
  `pkexec`, `mount`, `unshare` or `nsenter`; privileged (T2) tests run only in
  a disposable VM. If a task needs root, stop and say so.
- Every path from an image, context or Dockerfile is untrusted: reach it only
  through `rnk_rootfs::RootPath`, never `std::fs` or `Path::join` on a host
  path.
- Layer contents and digests never depend on `--jobs`, thread count,
  scheduling or host users. Timestamps follow kaniko: current time unless
  `--reproducible`, which uses a fixed epoch or `SOURCE_DATE_EPOCH`.
- New work starts from the latest `main` on its own branch:
  `git switch main && git pull --ff-only && git switch -c <type>/<scope>-<topic>`.
  Commit at every checkpoint (builds, `just check` passes, docs updated);
  push or merge only when the maintainer asks.
- After changing code, updating the affected docs in `docs/` and the repo
  root (`ARCHITECTURE.md`, design docs, crate cards, `CHANGELOG.md`, book…)
  is mandatory, on the same branch; a change without them is not done.
- Decisions go into a new ADR (`/adr`); never rewrite an accepted ADR. Never
  guess an undecided question: log it in `docs/open-questions.md` and mark
  the spot `[open: Q-NNN]`.
- Everything committed is English. The maintainer may write in Vietnamese.

## Claude Code setup

Project skills: `dev-loop`, `kaniko-parity`, `/adr`, `/sync-docs`, vendored
`security-audit` (milestones only). Subagents: `reviewer`, `host-safety`
(any diff in `rnk-rootfs`, `rnk-sandbox`, `rnk-snapshot`, `rnk-store`),
`kaniko-oracle` (kaniko source at `${KANIKO_SRC:-../kaniko}`). Durable
decisions belong in ADRs, not in auto memory.

Delegating to subagents (keeps their start-up cheap):

- `host-safety` and `kaniko-oracle` cannot run Bash, so they cannot query
  graphify. Run the queries yourself and put the results in their prompt:
  for `host-safety`, the changed files, `graphify affected`/`explain` output
  for the changed symbols, and the threat-model sections that apply; for
  `kaniko-oracle`, the kaniko checkout path and
  `graphify query "<question>" --graph <kaniko>/graphify-out/graph.json`
  output when that graph exists (build it with `graphify update <kaniko>`).
- `reviewer` gets only the scope and the requirement; it runs graphify
  itself.
- Pick the model by the task: `haiku` for locating code, `sonnet` for
  reading and summarising, `opus` only for review and host-safety judgement.
- Continue a finished agent with `SendMessage` rather than starting a new
  one for a follow-up question.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- When graphify-out/graph.json exists, run `graphify query "<question>"` before your first grep, search or source-file read for a codebase question, and put this rule in every subagent prompt that explores code and can run Bash (for read-only subagents, pass the query results instead). Read raw files only for lines the graph or an earlier read already located. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
- A committed graphify-out/graph.json is rewritten whole on every build, so a large diff to it in a pull request is expected. Do not flag it as churn or suggest skipping the graph rebuild or its hooks.
