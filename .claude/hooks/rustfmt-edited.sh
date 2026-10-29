#!/usr/bin/env bash
# PostToolUse hook (Edit|Write): format the Rust file Claude just changed.
#
# Reads the hook input JSON on stdin and runs `rustfmt --edition 2024` on
# `tool_input.file_path` when it is a `.rs` file inside the project. It never
# blocks or reports: a file that does not parse yet (halfway through a
# multi-step edit) is left alone, and `just check` reports it later.
set -u

command -v jq >/dev/null 2>&1 || exit 0
file=$(jq -r '.tool_input.file_path // empty' 2>/dev/null) || exit 0

case "$file" in
  *.rs) ;;
  *) exit 0 ;;
esac

project=${CLAUDE_PROJECT_DIR:-$PWD}
case "$file" in
  "$project"/*) ;;
  *) exit 0 ;;
esac
[ -f "$file" ] || exit 0

rustfmt=$(command -v rustfmt || echo "${CARGO_HOME:-$HOME/.cargo}/bin/rustfmt")
# Run from the project root so that rustup picks the toolchain pinned in
# rust-toolchain.toml.
cd "$project" && "$rustfmt" --edition 2024 --quiet -- "$file" >/dev/null 2>&1
exit 0
