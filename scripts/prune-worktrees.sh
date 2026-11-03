#!/usr/bin/env bash
# Removes agent worktrees under .claude/worktrees/ that hold nothing to lose,
# with their local branch (ADR-0096). Each carries its own target/ of several
# GB; target/ and other build output go with it.
#
# A worktree is removed only when all of these hold:
# - it is not locked, detached, on main, or the caller's own;
# - git status shows nothing (untracked files included), nothing ignored lies
#   outside target/, graphify-out/ and __pycache__/, and no VM run is live
#   (target/vm/run.*);
# - its branch is merged: the head of a pull request merged into main is the
#   branch's commit (asked through gh), or the branch has no commit of its
#   own and the worktree has been idle for a day (a fresh agent's branch
#   looks merged too, and Claude Code does not always lock its worktree).
#
# Usage: scripts/prune-worktrees.sh [--dry-run]
#   --dry-run only lists the decisions (it still fetches origin/main).
set -euo pipefail

dry_run=0
case "$#:${1:-}" in
  0:) ;;
  1:--dry-run) dry_run=1 ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

idle_seconds=${PRUNE_WORKTREES_IDLE_SECONDS:-86400}
caller=$(git rev-parse --path-format=absolute --show-toplevel)
main_wt=$(git worktree list --porcelain -z | tr '\0' '\n' | awk 'NR == 1 { print substr($0, 10) }')
cd "$main_wt"
git fetch -q origin main

# Prints why the worktree must stay, or nothing when it holds nothing to lose.
# Unusual names come back quoted and so are kept, the safe side.
dirty_reason() {
  local wt=$1 status entry
  if ! status=$(git --no-optional-locks -C "$wt" status --porcelain=v1 --untracked-files=normal \
      --ignored=traditional --ignore-submodules=none 2>&1); then
    echo "git status failed: ${status%%$'\n'*}"
    return
  fi
  while IFS= read -r entry; do
    case "$entry" in
      '' | '!! target/'* | '!! graphify-out/'* | '!! '*__pycache__/) ;;
      *) echo "has changes or files to keep (${entry:3})"; return ;;
    esac
  done <<< "$status"
  if compgen -G "$wt/target/vm/run.*" > /dev/null; then
    echo "a VM run may be live (target/vm/run.*)"
  fi
}

# Seconds since the epoch of the worktree's last checkout or commit (not the
# index, which any git status may rewrite); now when unknown, so an
# unreadable worktree counts as active.
last_activity() {
  local admin t=""
  if admin=$(git -C "$1" rev-parse --absolute-git-dir 2>/dev/null); then
    t=$({ stat -c %Y "$admin/HEAD" "$admin/logs/HEAD" 2>/dev/null || true; } | sort -n | tail -1)
  fi
  echo "${t:-$(date +%s)}"
}

# Prints why the branch is not merged, or nothing when it is.
unmerged_reason() {
  local wt=$1 branch=$2 sha=$3 heads idle
  if ! heads=$(timeout 60 gh pr list --state merged --base main --head "$branch" \
      --json headRefOid --jq '.[].headRefOid' < /dev/null 2>&1); then
    echo "GitHub not asked: ${heads%%$'\n'*}"
    return
  fi
  grep -Fxq "$sha" <<< "$heads" && return
  if git merge-base --is-ancestor "$sha" origin/main; then
    idle=$(( $(date +%s) - $(last_activity "$wt") ))
    [ "$idle" -ge "$idle_seconds" ] && return
    echo "no commit of its own, active $(( idle / 60 )) min ago (an agent may be starting)"
    return
  fi
  echo "branch $branch not merged into main"
}

removed=0
kept=0
keep() { echo "keep   $1: $2"; kept=$((kept + 1)); }

while IFS=$'\t' read -r -d '' locked ref wt; do
  case "$wt" in "$main_wt"/.claude/worktrees/*) ;; *) continue ;; esac
  name=${wt#"$main_wt"/.claude/worktrees/}
  branch=${ref#refs/heads/}
  if [ "$locked" = 1 ]; then keep "$name" "locked"; continue; fi
  if [ "$ref" = - ]; then keep "$name" "detached HEAD"; continue; fi
  if [ "$wt" = "$caller" ]; then keep "$name" "the caller's own worktree"; continue; fi
  if [ "$branch" = main ]; then keep "$name" "on main"; continue; fi
  reason=$(dirty_reason "$wt")
  if [ -n "$reason" ]; then keep "$name" "$reason"; continue; fi
  sha=$(git rev-parse --verify -q "$ref^{commit}") || { keep "$name" "cannot read $ref"; continue; }
  reason=$(unmerged_reason "$wt" "$branch" "$sha")
  if [ -n "$reason" ]; then keep "$name" "$reason"; continue; fi
  echo "remove $name ($branch)"
  removed=$((removed + 1))
  [ "$dry_run" = 1 ] && continue
  if ! git worktree remove "$wt"; then
    keep "$name" "git worktree remove failed"
    removed=$((removed - 1))
    continue
  fi
  # Deletes the branch only if no commit landed on it since the check.
  git update-ref -d "$ref" "$sha" || echo "note   $name: branch $branch moved, kept"
done < <(git worktree list --porcelain -z | awk -v RS='\0' '
  /^worktree / { wt = substr($0, 10); br = "-"; lk = 0 }
  /^branch /   { br = substr($0, 8) }
  /^locked/    { lk = 1 }
  /^$/         { if (wt != "") printf "%s\t%s\t%s%c", lk, br, wt, 0; wt = "" }
  END          { if (wt != "") printf "%s\t%s\t%s%c", lk, br, wt, 0 }')

verb=$([ "$dry_run" = 1 ] && echo "would remove" || echo "removed")
echo "prune-worktrees: $verb $removed, kept $kept"
