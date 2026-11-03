#!/usr/bin/env bash
# Tests scripts/prune-worktrees.sh in a throwaway repository with a fake `gh`
# on PATH: every worktree that holds something to lose must stay, only the
# merged and idle ones go, with their branch. Run: `just test-prune-worktrees`.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t

# The fake gh answers `pr list --state merged --base B --head H` from
# $tmp/merged, one "<base> <head> <sha>" line per merged pull request.
mkdir "$tmp/bin"
cat >"$tmp/bin/gh" <<'EOF'
#!/usr/bin/env bash
base="" head=""
while [ "$#" -gt 0 ]; do
  case "$1" in --base) base=$2; shift ;; --head) head=$2; shift ;; esac
  shift
done
[ -n "${FAKE_GH_FAIL:-}" ] && { echo "gh: not logged in" >&2; exit 4; }
awk -v b="$base" -v h="$head" '$1 == b && $2 == h { print $3 }' "$FAKE_GH_MERGED"
EOF
chmod +x "$tmp/bin/gh"
export FAKE_GH_MERGED="$tmp/merged"
: >"$FAKE_GH_MERGED"

git init -q --bare "$tmp/origin.git"
repo="$tmp/repo"
git init -q -b main "$repo"
cd "$repo"
echo a >tracked.txt
git add tracked.txt
git commit -q -m base
git remote add origin "$tmp/origin.git"
git push -q origin main
wts="$repo/.claude/worktrees"
mkdir -p "$wts"

add() { git worktree add -q -b "$1" "$wts/$2" main; }
commit_on() { git -C "$wts/$1" commit -q --allow-empty -m "$2"; git -C "$wts/$1" rev-parse HEAD; }
age() { # age <worktree>: make it look idle for two days
  local admin
  admin=$(git -C "$wts/$1" rev-parse --absolute-git-dir)
  touch -d '2 days ago' "$admin/HEAD" "$admin/logs/HEAD"
}

# Squash-merged: the pull request's head is the branch's commit.
add feat/squashed squashed
git config branch.feat/squashed.remote origin
git config branch.feat/squashed.merge refs/heads/feat/squashed
echo "main feat/squashed $(commit_on squashed one)" >>"$FAKE_GH_MERGED"
# The same, plus a commit after the merge.
add feat/after after
echo "main feat/after $(commit_on after one)" >>"$FAKE_GH_MERGED"
commit_on after two >/dev/null
# Merged into another base, not main.
add feat/stacked stacked
echo "other feat/stacked $(commit_on stacked one)" >>"$FAKE_GH_MERGED"
# Unmerged commit.
add feat/open open
commit_on open one >/dev/null
# No commit of its own: idle for two days, and fresh.
add feat/idle idle
age idle
add feat/fresh fresh
# Merged but with an untracked file, a tracked change, an ignored file
# outside target/, and a live VM run.
for w in untracked tracked ignored vmrun; do
  add "feat/$w" "$w"
  echo "main feat/$w $(commit_on "$w" one)" >>"$FAKE_GH_MERGED"
done
touch "$wts/untracked/new.txt"
echo b >"$wts/tracked/tracked.txt"
printf 'keep.me\n' >>"$(git -C "$wts/ignored" rev-parse --git-common-dir)/info/exclude"
touch "$wts/ignored/keep.me"
mkdir -p "$wts/vmrun/target/vm/run.123"
printf 'target/\n' >>"$(git rev-parse --git-common-dir)/info/exclude"
# Build output alone does not keep a merged worktree.
add feat/built built
echo "main feat/built $(commit_on built one)" >>"$FAKE_GH_MERGED"
mkdir -p "$wts/built/target/debug" && touch "$wts/built/target/debug/big"
# Locked, detached, detached and locked, on main, a path with a space.
add feat/locked locked
echo "main feat/locked $(commit_on locked one)" >>"$FAKE_GH_MERGED"
git worktree lock "$wts/locked"
git worktree add -q --detach "$wts/detached" main
git worktree add -q --detach "$wts/detached-locked" main
git worktree lock "$wts/detached-locked"
git switch -q --detach
git worktree add -q "$wts/on-main" main
add feat/space "with space"
echo "main feat/space $(commit_on "with space" one)" >>"$FAKE_GH_MERGED"

failures=0
check() { # check <yes|no> <worktree> <branch or -> <description>
  local want=$1 wt=$2 br=$3 what=$4 gone=no
  [ -d "$wts/$wt" ] || gone=yes
  if [ "$gone" != "$want" ]; then
    echo "FAIL: $what: worktree removed=$gone, want $want"
    failures=$((failures + 1))
  fi
  if [ "$br" != - ]; then
    local br_gone=no
    git show-ref -q --verify "refs/heads/$br" || br_gone=yes
    if [ "$br_gone" != "$want" ]; then
      echo "FAIL: $what: branch removed=$br_gone, want $want"
      failures=$((failures + 1))
    fi
  fi
}

run() { PATH="$tmp/bin:$PATH" bash "$root/scripts/prune-worktrees.sh" "$@"; }

out=$(run --dry-run)
for w in squashed idle built "with space"; do
  check no "$w" - "--dry-run leaves $w"
done
grep -q "would remove 4," <<<"$out" || { echo "FAIL: dry run summary: $out"; failures=$((failures + 1)); }

out=$(FAKE_GH_FAIL=1 run)
check no squashed feat/squashed "gh failing keeps a squash-merged worktree"
grep -q "GitHub not asked: gh: not logged in" <<<"$out" ||
  { echo "FAIL: gh failure not reported: $out"; failures=$((failures + 1)); }

if ! run --unknown >/dev/null 2>&1 && ! run --dry-run extra >/dev/null 2>&1; then :; else
  echo "FAIL: extra or unknown arguments accepted"; failures=$((failures + 1))
fi

out=$(run)
check yes squashed feat/squashed "squash-merged, head matches"
check yes idle feat/idle "no commit of its own, idle two days"
check yes built feat/built "merged with only build output"
check yes "with space" feat/space "path with a space"
check no after feat/after "a commit after the merge"
check no stacked feat/stacked "merged into another base"
check no open feat/open "unmerged commit"
check no fresh feat/fresh "no commit of its own, just created"
check no untracked feat/untracked "untracked file"
check no tracked feat/tracked "tracked change"
check no ignored feat/ignored "ignored file outside target/"
check no vmrun feat/vmrun "live VM run"
check no locked feat/locked "locked"
check no detached - "detached"
check no detached-locked - "detached and locked"
check no on-main main "on main"
grep -q "removed 4, kept 12" <<<"$out" || { echo "FAIL: summary: $out"; failures=$((failures + 1)); }
for reason in "detached: detached HEAD" "detached-locked: locked" "on-main: on main" \
    "vmrun: a VM run may be live" "stacked: branch feat/stacked not merged into main" \
    "fresh: no commit of its own"; do
  grep -qF "keep   $reason" <<<"$out" || { echo "FAIL: reason missing: $reason"; failures=$((failures + 1)); }
done
if git config --get-regexp '^branch\.feat/squashed\.' >/dev/null; then
  echo "FAIL: upstream settings of a removed branch left behind"; failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
  echo "$out"
  echo "prune-worktrees-test: $failures failure(s)"
  exit 1
fi
echo "prune-worktrees-test: ok"
