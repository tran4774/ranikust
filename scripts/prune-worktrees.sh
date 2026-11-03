#!/usr/bin/env bash
# Removes agent worktrees under .claude/worktrees/ whose branch is merged into
# origin/main, together with the local branch. Each worktree carries its own
# target/ of several GB, and finished agents leave theirs behind.
#
# Keeps a worktree that is locked (a running agent), has any change or
# untracked file, is detached, or whose branch is not merged. Nothing else is
# touched: the main checkout, worktrees elsewhere, remote branches.
#
# Usage: scripts/prune-worktrees.sh [--dry-run]
set -euo pipefail

dry_run=0
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=1 ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

# The first entry is the main checkout, even when run from inside a worktree.
main_wt=$(git worktree list --porcelain | awk 'NR == 1 { print $2 }')
cd "$main_wt"
git fetch -q origin main

merged() {
  git merge-base --is-ancestor "$1" origin/main && return 0
  # A squash merge is not an ancestor: accept a merged pull request whose head
  # is this exact commit, so commits made after the merge are never lost.
  local pr_head
  pr_head=$(gh pr list --state merged --head "$1" --json headRefOid \
    --jq '.[0].headRefOid // empty' 2>/dev/null) || return 1
  [ -n "$pr_head" ] && [ "$pr_head" = "$(git rev-parse "$1")" ]
}

removed=0
kept=0
while IFS=$'\t' read -r wt branch locked; do
  case "$wt" in "$main_wt"/.claude/worktrees/*) ;; *) continue ;; esac
  name=${wt##*/}
  reason=""
  if [ "$locked" = 1 ]; then
    reason="locked (a running agent)"
  elif [ -z "$branch" ]; then
    reason="detached HEAD"
  elif [ -n "$(git -C "$wt" status --porcelain)" ]; then
    reason="has changes or untracked files"
  elif ! merged "$branch"; then
    reason="branch $branch not merged"
  fi
  if [ -n "$reason" ]; then
    echo "keep   $name: $reason"
    kept=$((kept + 1))
    continue
  fi
  echo "remove $name ($branch)"
  removed=$((removed + 1))
  [ "$dry_run" = 1 ] && continue
  git worktree remove "$wt"
  # -D: a squash-merged branch is not an ancestor, but merged() checked it.
  git branch -q -D "$branch"
done < <(git worktree list --porcelain | awk '
  /^worktree / { wt = substr($0, 10); br = ""; lk = 0 }
  /^branch /   { br = substr($0, 19) }
  /^locked/    { lk = 1 }
  /^$/         { print wt "\t" br "\t" lk; wt = "" }
  END          { if (wt != "") print wt "\t" br "\t" lk }')

[ "$dry_run" = 1 ] || git worktree prune
verb=$([ "$dry_run" = 1 ] && echo "would remove" || echo "removed")
echo "prune-worktrees: $verb $removed, kept $kept"
