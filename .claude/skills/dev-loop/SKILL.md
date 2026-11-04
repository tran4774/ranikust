---
name: dev-loop
description: How to change Rust code in ranikust - check narrow then wide, write the failing test first for bug fixes, debug from evidence, and never claim done without fresh command output. Use whenever implementing, fixing or refactoring code in crates/.
---

# Development loop

The commands are in `CLAUDE.md`, `docs/agents/workflow.md#dev-loop` and the
`justfile`; this skill is about the order and the discipline. The ideas in
"Test first", "Debugging" and "Done means verified" are adapted from the
`test-driven-development`, `systematic-debugging` and
`verification-before-completion` skills of
[obra/superpowers](https://github.com/obra/superpowers) (MIT), commit
`2ce8aa1`. No text was copied.

## The loop: narrow to wide

0. Locate before you read: `graphify query "<question>"`, then `explain`
   or `affected "<symbol>"`, and read only the lines it names, in the same
   message as your other read-only calls (`git`, `sed -n`, `rg`). Exit 3
   (ambiguous): retry with `path::Symbol` or the node id listed; exit 4,
   or `No unique node match` from `affected`: `rg`. In an agent worktree
   run `graphify` from the main checkout (`CLAUDE.md#graphify`).
1. Edit. The PostToolUse hook runs `rustfmt` on each `.rs` file you touch.
   Comment why, not what, in one short line above the code it explains
   (`docs/agents/rules.md#comments`).
2. `cargo check -p <crate>` (or rust-analyzer diagnostics), then
   `cargo test -p <crate> <filter>`: only the tests for what you changed,
   then `cargo clippy -p <crate> --all-targets -- -D warnings`.
3. `just check` (fmt check, clippy and all T0 tests) before you report the
   task as done.
4. Only when relevant: `just check-aarch64` and `just test-aarch64`
   (syscalls, `cfg`, FFI, integer widths), `just deny` (dependencies),
   `just doc` (public items or doc comments), `just book` (`docs/book/`),
   `just ci` before opening a pull request. Anything that may run over
   five minutes runs in the background: wait for its notification instead
   of polling.

Fix a failure at the narrowest level that shows it, then widen again.

## Test first

- **Bug fix:** write a test that reproduces the bug, run it, and watch it fail
  for the reason you expect. Then fix the code and watch it pass. A test you
  never saw fail proves nothing.
- **New behaviour:** write the test for the smallest next piece, see it fail,
  write just enough code to pass, then tidy up with the tests green.
- Test behaviour through the public API of the crate, not private details.
- Tests run unprivileged and write only below a fresh temporary directory.
  Anything that needs user namespaces, mounts or root belongs to tier T1,
  T2c or T2 and carries its `#[ignore = "..."]` marker
  (`docs/testing.md#rules`). Run it only through its harness
  (`just test-container unit-c`, ADR-0063; `just test-vm unit`, the
  `vm-test` skill, which runs both), or say in your report that it was not
  run. Never run it on the host to "just check".

## Debugging

1. Reproduce the failure and read the whole error, including the first one in
   the output rather than the last.
2. Find where the behaviour diverges from what you expect before changing
   anything: read the code path, add a focused test or log line.
3. State one hypothesis, change one thing to test it, observe.
4. Fix the cause, not the symptom. Do not loosen a test, add a retry or a
   sleep, or silence a lint to make it pass.
5. After three failed fixes, stop and report what you know; the approach may
   be wrong.

## Done means verified

- Do not write "done", "fixed", "passes" or "works" unless you ran the
  command that shows it in this session, after your last edit, and read its
  output.
- Report what you ran and what it printed (the summary line is enough).
- Report what you could not verify, such as T1/T2 tests or behaviour that
  needs root, and why.
- If `just check` fails on something you did not touch, say so instead of
  working around it.
