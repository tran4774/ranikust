# --userns as root (#83, ADR-0003, ADR-0104), T2 only (the VM's quick
# suite): ranikust runs as real root with a `ranikust` range of its own in
# /etc/subuid and /etc/subgid, both restored after. Refused before
# anything is written without the range, with a range another line
# overlaps, with a state directory below a
# directory the range's first uid cannot search, with an existing state
# directory of root's, at or below root's default, and with a flag of kaniko mode
# alone; missing parents are made root's 0711, the build names --userns
# and leaves no probe directory; a RUN's uid 0 is the
# range's first uid on the host, by its uid_map, gid_map and status read
# from the host and by the owner there of a file it makes; the
# --reproducible digest equals the same build's without --userns; and
# ADR-0074's reach of a distinct host account is measured again: neither
# an account with the RUN's own ids nor one with the ids they map to on
# the host reaches it, the latter reaching a process of its own (the
# control). Then `prune --all --userns` empties the state directory
# as the range's first uid (pruned_as_the_range), the red team's trees
# and refusals in directories of its own (probe_left_by_a_killed_build,
# hostile_trees_pruned_as_the_range). In the kaniko leg, --userns with
# --mode=kaniko is a usage error.

# Below /var/lib, which only root writes and every uid may search.
UW=/var/lib/rnk-quick-userns

case_run() {
  [ "$ENGINE" = "sudo docker" ] || return 0
  if [ "${RANIKUST_QUICK_MODE:-}" = kaniko ]; then
    local log rc=0
    log="$(sudo "$RNK" --userns --mode=kaniko --context "$SMOKE_CASES_DIR/$CASE" --no-push 2>&1)" ||
      rc=$?
    expect "$rc" 2 "--userns with --mode=kaniko"
    grep -qF -- '--userns cannot be used with --mode=kaniko' <<<"$log" ||
      fail "--userns with --mode=kaniko: $log"
    return 0
  fi
  ! grep -qF " $UW" /proc/self/mountinfo || fail "mounts left under $UW by an earlier run"
  sudo rm -rf --one-file-system "$UW"
  sudo mkdir -m 755 "$UW" "$UW/ctx" "$UW/out"
  sudo install -m 644 "$SMOKE_CASES_DIR/$CASE/Dockerfile" "$UW/ctx/Dockerfile"
  local f
  for f in subuid subgid; do
    sudo touch "/etc/$f"
    sudo cp -p "/etc/$f" "$UW/$f.orig"
  done
  trap clean_up EXIT
  sudo sed -i '/^ranikust:/d' /etc/subuid /etc/subgid

  local log rc=0
  log="$(urun none --target content --state-dir "$UW/state" 2>&1)" || rc=$?
  expect "$rc" 1 "--userns without a ranikust range"
  grep -qF '/etc/subuid has no ranikust range' <<<"$log" &&
    grep -qF 'ranikust:<start>:65536 to both /etc/subuid and /etc/subgid' <<<"$log" ||
    fail "--userns without a ranikust range: $log"
  ! sudo test -e "$UW/state" || fail "--userns without a range made the state directory"

  # One past every range of both files, uids and gids apart.
  S="$(cat /etc/subuid /etc/subgid | awk -F: 'NF == 3 && $3 > 0 { e = $2 + $3; if (e > m) m = e }
    END { if (m < 1000000) m = 1000000; print int((m + 65535) / 65536) * 65536 }')"
  G=$((S + 65536))
  echo "ranikust:$S:65536" | sudo tee -a /etc/subuid >/dev/null
  echo "ranikust:$G:65536" | sudo tee -a /etc/subgid >/dev/null

  echo "rnk-quick-overlap:$((S + 1)):1" | sudo tee -a /etc/subuid >/dev/null
  rc=0
  log="$(urun overlap --target content --state-dir "$UW/overlap" 2>&1)" || rc=$?
  sudo sed -i '/^rnk-quick-overlap:/d' /etc/subuid
  expect "$rc" 1 "--userns with a range another range overlaps"
  grep -qF "the ranikust range $S:65536 of /etc/subuid overlaps line" <<<"$log" ||
    fail "--userns with a range another range overlaps: $log"
  ! sudo test -e "$UW/overlap" || fail "--userns made a state directory for an overlapped range"

  sudo mkdir -m 700 "$UW/closed"
  rc=0
  log="$(urun closed --target content --state-dir "$UW/closed/state" 2>&1)" || rc=$?
  expect "$rc" 1 "--userns below a directory its uid cannot search"
  grep -qF "$UW/closed is not searchable by uid $S and gid $G" <<<"$log" ||
    fail "--userns below a directory its uid cannot search: $log"
  ! sudo test -e "$UW/closed/state" || fail "--userns made a state directory it cannot search"

  sudo mkdir -m 700 "$UW/roots"
  rc=0
  log="$(urun roots --target content --state-dir "$UW/roots" 2>&1)" || rc=$?
  expect "$rc" 1 "--userns with a state directory of root's"
  grep -qF "$UW/roots is owned by uid 0, not $S" <<<"$log" ||
    fail "--userns with a state directory of root's: $log"
  expect "$(sudo stat -c '%u:%g %a' "$UW/roots") $(sudo ls -A "$UW/roots" | wc -l)" "0:0 700 0" \
    "root's state directory after --userns was refused"

  rc=0
  log="$(urun flagged --target content --state-dir "$UW/flagged" --ignore-path /srv 2>&1)" || rc=$?
  expect "$rc" 2 "--userns with a flag of kaniko mode alone"
  grep -qF -- '--ignore-path takes effect in kaniko mode only' <<<"$log" &&
    grep -qF -- 'drop it: --userns builds in overlay mode only' <<<"$log" ||
    fail "--userns with a flag of kaniko mode alone: $log"
  ! sudo test -e "$UW/flagged" || fail "--userns made a state directory before its usage error"

  local before
  before="$(sudo stat -c '%u:%g %a' /var/lib/ranikust 2>/dev/null || echo missing)"
  rc=0
  log="$(urun default --target content 2>&1)" || rc=$?
  expect "$rc" 1 "--userns with root's default state directory"
  grep -qF "the state directory /var/lib/ranikust is root's default:" <<<"$log" ||
    fail "--userns with root's default state directory: $log"
  expect "$(sudo stat -c '%u:%g %a' /var/lib/ranikust 2>/dev/null || echo missing)" "$before" \
    "root's default state directory after --userns was refused"
  rc=0
  log="$(urun below --target content --state-dir /var/lib/ranikust/userns 2>&1)" || rc=$?
  expect "$rc" 1 "--userns below root's default state directory"
  grep -qF "/var/lib/ranikust/userns is below root's default /var/lib/ranikust:" <<<"$log" ||
    fail "--userns below root's default state directory: $log"
  expect "$(sudo stat -c '%u:%g %a' /var/lib/ranikust 2>/dev/null || echo missing)" "$before" \
    "root's default state directory after --userns below it was refused"

  # Missing parents are root's, searchable only.
  rc=0
  log="$(urun nested --target content --state-dir "$UW/new/a/state" 2>&1)" || rc=$?
  expect "$rc" 0 "--userns below missing parents: $log"
  grep -qF "ranikust: overlay mode with --userns, in the build's user namespace of the ranikust range" \
    <<<"$log" || fail "the --userns build's mode line: $log"
  expect "$(sudo stat -c '%u:%g %a' "$UW/new" "$UW/new/a" "$UW/new/a/state" | paste -sd ' ')" \
    "0:0 711 0:0 711 $S:$G 700" "the --userns state directory and the parents it made"
  expect "$(sudo find "$UW" -maxdepth 4 \( -name '.ranikust-anon-userns-probe-*' \
    -o -name '.ranikust-new-*' \) | wc -l)" 0 "probe and unnamed directories --userns left"

  local mapped plain
  mapped="$(udigest userns --reproducible --target content \
    --destination "localhost/rnk-case:$CASE-userns")"
  expect "$(sudo stat -c '%u:%g %a' "$UW/state")" "$S:$G 700" "the state directory --userns made"
  plain="$(build plain --reproducible --target content)"
  expect "$mapped" "$plain" "the --userns digest against the same build's without it"
  sudo docker load -q -i "$UW/out/userns/image.tar" >/dev/null || fail "load the --userns image"
  expect "$(sudo docker run --rm "localhost/rnk-case:$CASE-userns" \
    sh -c 'stat -c "%u:%g %a" /home/u/f /d /bin/suid && readlink /home/u/l')" \
    "$(printf '1234:1234 644\n0:0 2750\n0:0 4755\nf')" "the --userns image's owners and modes"
  seen_from_the_host
  pruned_as_the_range
  probe_left_by_a_killed_build
  hostile_trees_pruned_as_the_range
}

# pruned_as_the_range: `prune --all --userns` (ADR-0146) empties the
# state directory the builds left as the range's first uid, in its user
# namespace: root's own `prune --all` names it; it refuses root's default,
# a state directory of root's and one below a directory the range's uid
# cannot search, each left as it was, and root's `prune --all` does not
# send root back to it for the last; with --metrics-file it is a usage
# error; a directory of root's put in the store and a tmpfs mounted at
# one of its entries stay, the prune names them and prints root's
# commands, a build is refused, naming it, until it finishes; those
# commands, run as root, change nothing while an entry's uid or gid is
# neither root's nor the range's, leave the tmpfs and the marker, then,
# once it is unmounted, all but the marker, which the prune then empties.
pruned_as_the_range() {
  local log rc=0
  log="$(sudo "$RNK" prune --all --state-dir "$UW/state" 2>&1)" || rc=$?
  expect "$rc" 1 "root's prune --all on the --userns state directory"
  grep -qF -- "ranikust prune --all --userns --state-dir $UW/state" <<<"$log" ||
    fail "root's prune --all on the --userns state directory: $log"

  local before
  before="$(sudo stat -c '%u:%g %a' /var/lib/ranikust 2>/dev/null || echo missing)"
  rc=0
  log="$(sudo "$RNK" prune --all --userns 2>&1)" || rc=$?
  expect "$rc" 1 "prune --all --userns at root's default"
  grep -qF "the state directory /var/lib/ranikust is root's default" <<<"$log" ||
    fail "prune --all --userns at root's default: $log"
  expect "$(sudo stat -c '%u:%g %a' /var/lib/ranikust 2>/dev/null || echo missing)" "$before" \
    "root's default state directory after prune --all --userns was refused"

  rc=0
  log="$(sudo "$RNK" prune --all --userns --state-dir "$UW/roots" 2>&1)" || rc=$?
  expect "$rc" 1 "prune --all --userns on a state directory of root's"
  grep -qF "$UW/roots is owned by uid 0, not $S" <<<"$log" ||
    fail "prune --all --userns on a state directory of root's: $log"
  expect "$(sudo stat -c '%u:%g %a' "$UW/roots") $(sudo ls -A "$UW/roots" | wc -l)" "0:0 700 0" \
    "root's state directory after prune --all --userns was refused"

  sudo install -d -m 700 -o "$S" -g "$G" "$UW/closed/st"
  sudo install -m 600 -o "$S" -g "$G" /dev/null "$UW/closed/st/.ranikust-state"
  rc=0
  log="$(sudo "$RNK" prune --all --userns --state-dir "$UW/closed/st" 2>&1)" || rc=$?
  expect "$rc" 1 "prune --all --userns below a directory its uid cannot search"
  grep -qF "$UW/closed is not searchable by uid $S and gid $G" <<<"$log" ||
    fail "prune --all --userns below a directory its uid cannot search: $log"
  expect "$(sudo ls -A "$UW/closed/st")" ".ranikust-state" \
    "the state directory prune --all --userns could not search to"
  rc=0
  log="$(sudo "$RNK" prune --all --state-dir "$UW/closed/st" 2>&1)" || rc=$?
  expect "$rc" 1 "root's prune --all below a directory the range's uid cannot search"
  ! grep -qF -- "--userns" <<<"$log" ||
    fail "root's prune --all sent root back to prune --all --userns: $log"

  rc=0
  log="$(sudo "$RNK" prune --all --userns --state-dir "$UW/state" --metrics-file "$UW/m.json" 2>&1)" ||
    rc=$?
  expect "$rc" 2 "prune --all --userns with --metrics-file"
  grep -qF -- "--metrics-file is refused with prune --userns" <<<"$log" ||
    fail "prune --all --userns with --metrics-file: $log"
  sudo test ! -e "$UW/m.json" || fail "prune --all --userns wrote a metrics report"

  sudo test -d "$UW/state/builds" || fail "no builds/ in the --userns state directory"
  sudo mkdir -m 755 "$UW/state/builds/rnk-roots"
  sudo mkdir -p "$UW/state/tmp"
  sudo touch "$UW/state/builds/rnk-roots/f"
  sudo mount -t tmpfs rnk-userns-left "$UW/state/tmp"
  sudo touch "$UW/state/tmp/keep"
  rc=0
  log="$(sudo "$RNK" prune --all --userns --state-dir "$UW/state" 2>&1)" || rc=$?
  expect "$rc" 1 "prune --all --userns with a directory of root's in the store"
  local way
  way="$(grep -F "Stop every --userns build first, and unmount anything below $UW/state;" <<<"$log")" &&
    grep -qF "( eval '" <<<"$way" ||
    fail "prune --all --userns with a directory of root's in the store: $log"
  grep -qF "ranikust: the build's user namespace: shifted range" <<<"$log" &&
    grep -F 'it still holds ' <<<"$log" | grep -F '"builds"' | grep -qF '"tmp"' ||
    fail "prune --all --userns with a directory of root's in the store: $log"
  sudo test -f "$UW/state/builds/rnk-roots/f" || fail "prune --all --userns removed a file of root's"
  rc=0
  log="$(urun cut --target content --state-dir "$UW/state" 2>&1)" || rc=$?
  expect "$rc" 1 "a --userns build in a state directory a prune did not finish"
  grep -qF -- "ranikust prune --all --userns --state-dir $UW/state" <<<"$log" ||
    fail "a --userns build in a state directory a prune did not finish: $log"

  # The commands it prints, run as root: the marker and the tmpfs mounted
  # at an entry stay, with what it holds, the rest goes; once unmounted,
  # all but the marker.
  way="( eval '${way#*"( eval '"}"
  way="${way%"; then run ranikust prune --all --userns --state-dir $UW/state again"}"
  # An entry of an id neither root's nor the range's, by uid, then by gid
  # alone, just below the range and just past it: nothing changes.
  local foreign listed
  for foreign in "$((S - 1)):$G" "$((S + 65536)):$G" "$S:$((G - 1))" "$S:$((G + 65536))"; do
    sudo install -m 644 -o "${foreign%:*}" -g "${foreign#*:}" /dev/null \
      "$UW/state/builds/rnk-roots/foreign"
    listed="$(sudo find "$UW/state" -printf '%P %U:%G %m\n' | sort)"
    rc=0
    log="$(sudo bash -c "$way" 2>&1)" || rc=$?
    expect "$rc" 1 "the printed commands with an entry of $foreign"
    grep -qF "nothing changed: entries whose uid or gid is neither 0 nor of the ranikust range (uids $S-" <<<"$log" &&
      grep -qF "(--userns builds can use another --state-dir), or find failed" <<<"$log" ||
      fail "the printed commands with an entry of $foreign: $log"
    expect "$(sudo find "$UW/state" -printf '%P %U:%G %m\n' | sort)" "$listed" \
      "the state directory after the printed commands met an entry of $foreign"
    sudo rm "$UW/state/builds/rnk-roots/foreign"
  done
  rc=0
  sudo bash -c "$way" || rc=$?
  expect "$rc" 1 "the printed commands with a tmpfs mounted at an entry"
  expect "$(sudo ls -A "$UW/state" | tr '\n' ' ')" ".ranikust-pruning tmp " \
    "the state directory after the printed commands, a tmpfs mounted at an entry"
  sudo test -f "$UW/state/tmp/keep" || fail "the printed commands removed a file on another mount"
  sudo umount "$UW/state/tmp"
  sudo bash -c "$way" || fail "the printed commands: $way"
  expect "$(sudo ls -A "$UW/state")" ".ranikust-pruning" "the state directory after the printed commands"
  rc=0
  log="$(sudo "$RNK" prune --all --userns --state-dir "$UW/state" 2>&1)" || rc=$?
  expect "$rc" 0 "prune --all --userns: $log"
  grep -qxF "emptied $UW/state" <<<"$log" || fail "prune --all --userns: $log"
  expect "$(sudo stat -c '%u:%g %a' "$UW/state") $(sudo ls -A "$UW/state" | wc -l)" "$S:$G 700 0" \
    "the state directory prune --all --userns emptied"
  log="$(sudo "$RNK" prune --all --userns --state-dir "$UW/state" 2>&1)"
  grep -qxF "nothing to prune: $UW/state is empty" <<<"$log" || fail "prune --all --userns again: $log"
  sudo rmdir "$UW/state"
}

# ustate DIR: a --userns state directory at DIR as its builds leave it,
# made by hand: the range's first ids own it, its store's directories, its
# marker and a blob.
ustate() {
  sudo rm -rf --one-file-system "$1"
  sudo install -d -m 700 -o "$S" -g "$G" "$1" "$1/blobs" "$1/blobs/sha256" "$1/caches" \
    "$1/builds" "$1/tmp"
  sudo install -m 600 -o "$S" -g "$G" /dev/null "$1/.ranikust-state"
  sudo install -m 644 -o "$S" -g "$G" /dev/null "$1/blobs/sha256/$(printf '07%.0s' {1..32})"
}

# tree_of DIR: every entry below DIR with its type, owner, mode, size and
# symlink target, sorted; no link count, which a removed hardlink lowers.
tree_of() {
  sudo find "$1" -printf '%P %y %U:%G %m %s %l\n' | sort
}

# printed_way DIR LOG: root's commands, as root runs them, that LOG, from
# an unfinished prune --all --userns of DIR, prints.
printed_way() {
  local way
  way="$(grep -F "Stop every --userns build first, and unmount anything below $1;" <<<"$2")" &&
    grep -qF "( eval '" <<<"$way" || return 1
  way="( eval '${way#*"( eval '"}"
  printf '%s\n' "${way%"; then run ranikust prune --all --userns --state-dir $1 again"}"
}

# probe_left_by_a_killed_build (T4, #207): a --userns build killed once it
# made its probe's directory, root's 0700, empty, or the range's holding
# root's empty 0700 overlay directory: prune --all --userns ends emptied,
# through root's printed commands where its uid cannot remove it, a
# second run finding it empty: no loop. With no store yet, both prunes
# refuse the directory, changing nothing, prune --all --userns saying a
# --userns build removes empty probe directories of root's and the
# range's (not one that holds a file), and the next --userns build takes
# it, removing them first.
probe_left_by_a_killed_build() {
  local d="$UW/probe" p probe log rc way listed
  for p in root given; do
    ustate "$d"
    probe="$d/.ranikust-anon-userns-probe-1-$p"
    if [ "$p" = root ]; then
      sudo install -d -m 700 -o 0 -g 0 "$probe"
    else
      sudo install -d -m 700 -o "$S" -g "$G" "$probe"
      sudo install -d -m 700 -o 0 -g 0 "$probe/overlay"
    fi
    rc=0
    log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)" || rc=$?
    echo "  $CASE: measured: prune --all --userns, a probe directory left ($p): exit $rc: $log" >&2
    if [ "$rc" = 1 ]; then
      grep -qF ".ranikust-anon-userns-probe-1-$p" <<<"$log" ||
        fail "prune --all --userns, a probe directory left ($p), names it not: $log"
      way="$(printed_way "$d" "$log")" || fail "no root's commands ($p): $log"
      sudo timeout 120 bash -c "$way" || fail "root's commands, a probe directory left ($p)"
      expect "$(sudo ls -A "$d")" ".ranikust-pruning" "after root's commands ($p)"
      rc=0
      log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)" || rc=$?
    fi
    expect "$rc" 0 "prune --all --userns, a probe directory left ($p): $log"
    grep -qxF "emptied $d" <<<"$log" || fail "prune --all --userns ($p): $log"
    log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)"
    grep -qxF "nothing to prune: $d is empty" <<<"$log" || fail "prune again ($p): $log"
    sudo rmdir "$d"
  done
  # No store yet: the build was killed before it made one.
  sudo install -d -m 700 -o "$S" -g "$G" "$d"
  sudo install -d -m 700 -o 0 -g 0 "$d/.ranikust-anon-userns-probe-2-root"
  sudo install -d -m 700 -o "$S" -g "$G" "$d/.ranikust-anon-userns-probe-3-range"
  # One that holds a file, which no build removes: no word of the probe.
  sudo touch "$d/.ranikust-anon-userns-probe-2-root/f"
  rc=0
  log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)" || rc=$?
  expect "$rc" 1 "prune --all --userns, a probe directory that holds a file"
  ! grep -qF "probe director" <<<"$log" ||
    fail "prune --all --userns, a probe directory that holds a file: $log"
  sudo rm "$d/.ranikust-anon-userns-probe-2-root/f"
  listed="$(tree_of "$d")"
  rc=0
  log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)" || rc=$?
  echo "  $CASE: measured: prune --all --userns, only probe directories: exit $rc: $log" >&2
  expect "$rc" 1 "prune --all --userns, only probe directories"
  grep -qF "so ranikust prune --all --userns removes nothing" <<<"$log" &&
    grep -qF "it holds only probe directories a --userns build left" <<<"$log" &&
    grep -qF "a --userns build with --state-dir $d removes them" <<<"$log" ||
    fail "prune --all --userns, only probe directories: $log"
  expect "$(tree_of "$d")" "$listed" "after prune --all --userns, only a probe directory"
  rc=0
  log="$(sudo timeout 120 "$RNK" prune --all --state-dir "$d" 2>&1)" || rc=$?
  echo "  $CASE: measured: root's prune --all, only a probe directory of root's: exit $rc: $log" >&2
  expect "$rc" 1 "root's prune --all, only a probe directory of root's"
  expect "$(tree_of "$d")" "$listed" "after root's prune --all, only a probe directory"
  rc=0
  log="$(urun probe --target content --state-dir "$d" 2>&1)" || rc=$?
  expect "$rc" 0 "a --userns build, only a probe directory of root's: $log"
  ! sudo test -e "$d/.ranikust-anon-userns-probe-2-root" ||
    fail "a --userns build left root's probe directory"
  ! sudo test -e "$d/.ranikust-anon-userns-probe-3-range" ||
    fail "a --userns build left the range's probe directory"
  rc=0
  log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)" || rc=$?
  expect "$rc" 0 "prune --all --userns after that build: $log"
  sudo rmdir "$d"
}

# hostile_trees_pruned_as_the_range (T4, ADR-0146): in a --userns state
# directory, what root and ids outside the range could leave: root's
# directories holding a file, open and closed, a file and a setuid file of
# root's, directories of the ids just below and just past the range
# holding one, symlinks out (one at a store's name), a hardlink to root's
# 0600 file, a FIFO, a bind mount of the same filesystem holding the
# range's file and an account's. prune --all --userns removes no file
# outside, nothing below the bind mount and nothing those directories
# hold, and never blocks; root's commands then change nothing while
# those ids' directories are there, nor while the bind mount is, its
# account's entry being another id's, and finish once it is unmounted.
# Refused, nothing changed: a state directory group may write, one a
# build holds (its flock), one holding a FIFO .ranikust-building, and
# where no user namespace can be made.
hostile_trees_pruned_as_the_range() {
  local d="$UW/hostile" v="$UW/victim" c="$UW/hostile/caches" log rc way id listed victims
  local below=$((S - 1)) past=$((S + 65536))
  sudo rm -rf --one-file-system "$v"
  sudo install -d -m 755 "$v" "$v/dir"
  printf secret | sudo tee "$v/secret" >/dev/null && sudo chmod 600 "$v/secret"
  printf keep | sudo tee "$v/dir/keep" >/dev/null
  sudo install -d -m 755 -o "$S" -g "$G" "$v/bm"
  sudo install -m 644 -o "$S" -g "$G" /dev/null "$v/bm/ranges"
  sudo install -m 644 -o "$(id -u)" -g "$(id -g)" /dev/null "$v/bm/accounts"
  ustate "$d"
  sudo install -d -m 755 -o 0 -g 0 "$c/root-open"
  sudo install -m 644 -o 0 -g 0 /dev/null "$c/root-open/f"
  sudo install -d -m 700 -o 0 -g 0 "$c/root-closed"
  sudo install -m 644 -o 0 -g 0 /dev/null "$c/root-closed/f"
  sudo install -m 600 -o 0 -g 0 /dev/null "$c/root-file"
  sudo install -m 4755 -o 0 -g 0 /bin/true "$c/root-suid"
  for id in "$below" "$past"; do
    sudo install -d -m 755 -o "$id" -g "$G" "$c/d-$id"
    sudo install -m 644 -o "$id" -g "$G" /dev/null "$c/d-$id/f"
  done
  sudo ln -s "$v/dir" "$c/link-out"
  sudo ln -s "$v/dir" "$d/layers"
  sudo ln "$v/secret" "$c/hard-secret"
  sudo mkfifo -m 600 "$c/fifo" && sudo chown -h "$S:$G" "$c/fifo"
  sudo install -d -m 700 -o "$S" -g "$G" "$c/bm"
  sudo mount --bind "$v/bm" "$c/bm"
  expect "$(sudo stat -c %d "$c/bm")" "$(sudo stat -c %d "$c")" "the bind mount's filesystem"
  victims="$(tree_of "$v")"
  rc=0
  log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)" || rc=$?
  echo "  $CASE: measured: prune --all --userns in a hostile tree: exit $rc; left: $(sudo find "$d" -mindepth 1 -printf '%P %U:%G %m, ')" >&2
  expect "$rc" 1 "prune --all --userns in a hostile tree: $log"
  expect "$(tree_of "$v")" "$victims" "outside, after prune --all --userns"
  expect "$(sudo cat "$v/secret" "$v/dir/keep")" "secretkeep" "the files outside"
  for id in root-open/f root-closed/f "d-$below/f" "d-$past/f" bm/ranges bm/accounts; do
    sudo test -f "$c/$id" || fail "prune --all --userns removed $id"
  done
  way="$(printed_way "$d" "$log")" || fail "no root's commands: $log"
  listed="$(tree_of "$d")"
  rc=0
  log="$(sudo timeout 120 bash -c "$way" 2>&1)" || rc=$?
  expect "$rc" 1 "root's commands, directories of ids outside the range there"
  grep -qF "nothing changed: entries whose uid or gid is neither 0 nor of the ranikust range" \
    <<<"$log" || fail "root's commands, directories of ids outside the range there: $log"
  expect "$(tree_of "$d")" "$listed" "after root's commands met ids outside the range"
  sudo rm -r "$c/d-$below" "$c/d-$past"
  listed="$(tree_of "$d")"
  rc=0
  log="$(sudo timeout 120 bash -c "$way" 2>&1)" || rc=$?
  expect "$rc" 1 "root's commands, a bind mount below holding an account's entry"
  grep -qF "nothing changed" <<<"$log" ||
    fail "root's commands, a bind mount below holding an account's entry: $log"
  expect "$(tree_of "$d")" "$listed" "after root's commands met the bind mount"
  expect "$(tree_of "$v")" "$victims" "outside, after root's commands met the bind mount"
  sudo umount "$c/bm"
  sudo timeout 120 bash -c "$way" || fail "root's commands, the bind mount gone"
  expect "$(sudo ls -A "$d")" ".ranikust-pruning" "after root's commands"
  expect "$(tree_of "$v")" "$victims" "outside, after root's commands"
  rc=0
  log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)" || rc=$?
  expect "$rc" 0 "prune --all --userns after root's commands: $log"
  sudo rmdir "$d"

  # Refusals: nothing changed, the marker kept.
  local how
  for how in group-writable held building no-userns; do
    ustate "$d"
    case "$how" in
      group-writable) sudo chmod 770 "$d" ;;
      held)
        sudo flock -s "$d" sleep 21.25 &
        rc=0
        until pgrep -x -f 'sleep 21.25' >/dev/null; do
          rc=$((rc + 1)) && [ "$rc" -lt 100 ] || fail "no process holds $d's flock"
          sleep 0.1
        done
        ;;
      building) sudo mkfifo -m 600 "$d/.ranikust-building" ;;
      no-userns)
        USERNS_MAX="$(sysctl -n user.max_user_namespaces)"
        sudo sysctl -qw user.max_user_namespaces=0
        ;;
    esac
    listed="$(sudo stat -c '%u:%g %a' "$d") $(tree_of "$d")"
    rc=0
    log="$(sudo timeout 120 "$RNK" prune --all --userns --state-dir "$d" 2>&1)" || rc=$?
    case "$how" in
      held) sudo pkill -x -f 'sleep 21.25' && wait ;;
      no-userns) sudo sysctl -qw "user.max_user_namespaces=$USERNS_MAX" && USERNS_MAX= ;;
    esac
    echo "  $CASE: measured: prune --all --userns refused ($how): exit $rc: $log" >&2
    expect "$rc" 1 "prune --all --userns, $how"
    [ "$how" != held ] ||
      grep -qF "ranikust prune --all --userns empties it only while no build runs there" <<<"$log" ||
      fail "prune --all --userns, $how: $log"
    expect "$(sudo stat -c '%u:%g %a' "$d") $(tree_of "$d")" "$listed" \
      "the state directory after prune --all --userns was refused ($how)"
  done
  sudo rm -rf --one-file-system "$d" "$v"
}

# seen_from_the_host: the `seen` stage's RUNs, from the host while each
# waits: as root, its maps, ids and a file it made; as USER 1234:1234, the
# reach of a host account with its ids and of one with the ids they map
# to (ADR-0074), none.
seen_from_the_host() {
  local o="$CASES_OUT/$CASE/seen" pid bg rc=0
  rm -rf "$o" && mkdir -p "$o"
  urun seen --target seen --state-dir "$UW/state" >"$o/build.log" 2>&1 &
  bg=$!
  pid="$(waiting root)" || fail "no RUN waited as root: $(cat "$o/build.log")"
  expect "$(awk '{ print $1, $2, $3 }' "/proc/$pid/uid_map")" "0 $S 65536" "the RUN's uid_map"
  expect "$(awk '{ print $1, $2, $3 }' "/proc/$pid/gid_map")" "0 $G 65536" "the RUN's gid_map"
  expect "$(awk '/^(Uid|Gid):/ { print $2, $3, $4, $5 }' "/proc/$pid/status" | paste -sd ' ')" \
    "$S $S $S $S $G $G $G $G" "the RUN's ids on the host"
  expect "$(sudo stat -c '%u:%g' "/proc/$pid/root/made-by-run")" "$S:$G" \
    "the owner on the host of a file the RUN made"
  sudo rm "/proc/$pid/root/tmp/wait-root"

  pid="$(waiting user)" || fail "no RUN waited as USER 1234: $(cat "$o/build.log")"
  expect "$(awk '/^Uid:/ { print $2 }' "/proc/$pid/status")" "$((S + 1234))" \
    "the USER 1234 RUN's uid on the host"
  local own far ctl owner ids
  own="$(reach "$pid" 1234 1234)"
  far="$(reach "$pid" $((S + 1234)) $((G + 1234)))"
  # The kernel shows a process's /proc files as its own only while it is
  # dumpable, else as root of its memory's user namespace.
  owner="$(stat -c '%u:%g' "/proc/$pid/environ")"
  ids="$(awk '/^(Uid|Gid):/ { print $2, $3, $4, $5 }' "/proc/$pid/status" | paste -sd ' ')"
  ctl="$(control_reach $((S + 1234)) $((G + 1234)))"
  echo "  $CASE: measured (ADR-0074): a host account 1234:1234 reaches:$own; one $((S + 1234)):$((G + 1234)) reaches:$far (its own process:$ctl); the RUN's ids $ids, its /proc files owned by $owner" >&2
  # Dumpable, so that only the user namespace can refuse the account.
  expect "$owner" "$((S + 1234)):$((G + 1234))" "the USER 1234 RUN is dumpable"
  expect "$ctl" " environ root" "what a host account reaches of a process of its own (the control)"
  expect "$own" " nothing" "what a host account with the RUN's own ids reaches"
  expect "$far" " nothing" "what a host account with the ids the RUN's map to reaches"
  sudo rm "/proc/$pid/root/tmp/wait-user"
  wait "$bg" || rc=$?
  expect "$rc" 0 "the seen build: $(cat "$o/build.log")"
}

# waiting WHO: the pid of the RUN waiting as WHO (root, user), once its
# file shows; none after 60 s.
waiting() {
  local i=0 pid
  while [ "$i" -lt 600 ]; do
    pid="$(pgrep -o -f -- "rnk-userns-wait-$1" || true)"
    if [ -n "$pid" ] && sudo test -e "/proc/$pid/root/tmp/wait-$1"; then
      echo "$pid"
      return 0
    fi
    sleep 0.1
    i=$((i + 1))
  done
  return 1
}

# reach PID UID GID: what of the process PID a host process of UID and GID,
# with no supplementary group, reads: its environment, its root.
reach() {
  local r="" as=(sudo setpriv --reuid="$2" --regid="$3" --clear-groups --)
  "${as[@]}" cat "/proc/$1/environ" >/dev/null 2>&1 && r+=" environ"
  "${as[@]}" ls "/proc/$1/root/" >/dev/null 2>&1 && r+=" root"
  echo "${r:- nothing}"
}

# control_reach UID GID: what a host process of UID and GID reaches of a
# process of its own ids, so that " nothing" from reach means the kernel
# refused, not that setpriv failed.
control_reach() {
  local pid r i=0
  sudo timeout 30 setpriv --reuid="$1" --regid="$2" --clear-groups -- sleep 1234.5 &
  while [ "$i" -lt 50 ]; do
    pid="$(pgrep -o -x -f 'sleep 1234.5' || true)"
    [ -n "$pid" ] && [ "$(awk '/^Uid:/ { print $2 }' "/proc/$pid/status" 2>/dev/null)" = "$1" ] &&
      break
    pid=""
    sleep 0.1
    i=$((i + 1))
  done
  if [ -n "$pid" ]; then
    r="$(reach "$pid" "$1" "$2")"
    sudo kill "$pid" || true
  fi
  wait "$!" 2>/dev/null || true
  echo "${r:- no process}"
}

# urun NAME ARG...: ranikust as root with --userns, its working directory,
# its context and its outputs ($UW/out/NAME, the range's first ids') where
# those ids reach them; prints the log, returns the exit code.
urun() {
  local o="$UW/out/$1"
  shift
  sudo rm -rf "$o" && sudo mkdir -m 700 "$o" && sudo chown "${S:-0}:${G:-0}" "$o"
  # The default --dockerfile is looked for from here first.
  (cd "$UW/ctx" && sudo "$RNK" --userns --context "$UW/ctx" --no-push \
    --build-arg "BASE=$BASE" --oci-layout-path "$o/oci" --tar-path "$o/image.tar" "$@")
}

# udigest NAME ARG...: urun into the state directory $UW/state; prints
# the digest.
udigest() {
  local name="$1" log rc=0
  shift
  log="$(urun "$name" --state-dir "$UW/state" "$@" 2>&1)" || rc=$?
  if [ "$rc" != 0 ] || ! grep -Eq '^sha256:[0-9a-f]{64}$' <<<"$log"; then
    echo "$log" >&2
    fail "--userns build $name exited $rc" || return 1
  fi
  grep -Eo '^sha256:[0-9a-f]{64}$' <<<"$log" | tail -1
}

# clean_up: no RUN left waiting, and the id files as they were.
clean_up() {
  local p
  ! grep -qF " $UW/state/tmp " /proc/self/mountinfo || sudo umount "$UW/state/tmp" || true
  ! grep -qF " $UW/hostile/caches/bm " /proc/self/mountinfo ||
    sudo umount "$UW/hostile/caches/bm" || true
  [ -z "${USERNS_MAX:-}" ] || sudo sysctl -qw "user.max_user_namespaces=$USERNS_MAX" || true
  sudo pkill -x -f 'sleep 21.25' || true
  for p in $(pgrep -f -- 'rnk-userns-wait-' || true); do
    sudo rm -f "/proc/$p/root/tmp/wait-root" "/proc/$p/root/tmp/wait-user" || true
  done
  wait || true
  for f in subuid subgid; do
    sudo cp -p "$UW/$f.orig" "/etc/$f" || echo "error: $CASE: /etc/$f not restored" >&2
  done
}
