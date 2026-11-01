# Independent stages built at once (ADR-0013, ADR-0059): with
# --reproducible, the image is the same at --jobs 1 and 4, and the same as
# the one built with the stages forced one after the other
# (Dockerfile.chain). In the VM with RANIKUST_QUICK_BENCH=1, also the
# acceptance criterion 7 bench: wall time and peak memory of the chained
# (sequential) build against the independent stages at --jobs 1 and the
# default, in kaniko mode off / and in overlay mode, capped to the e2e
# builders' budget (4 CPUs, 8 GiB) and uncapped (bench.md in the output).
case_run() {
  local d1 d4 dc got
  d1="$(build par1 --reproducible --jobs 1)"
  d4="$(build par4 --reproducible --jobs 4)"
  dc="$(build chain -f Dockerfile.chain --reproducible --jobs 4)"
  expect "$d4" "$d1" "digest at --jobs 4 and --jobs 1"
  expect "$dc" "$d1" "digest of the chained stages and the independent ones"
  got="$(run par4)"
  [[ $got =~ ^([0-9a-f]{16})\ \1\ \1\ \1\ $ ]] || fail "the image printed $(printf %q "$got")"
  if [ "${RANIKUST_QUICK_BENCH:-}" = 1 ] && [ "$ENGINE" = "sudo docker" ]; then
    case_bench
  fi
}

# The bench: real root on the VM. Each build runs in a transient systemd
# scope (capped: CPUQuota=400%, AllowedCPUs = the last 4 vCPUs,
# MemoryMax=MemorySwapMax=8G, as the e2e builders; uncapped: no limit),
# which records the scope's memory.peak (page cache included) when the
# build ends. One warm-up round, then RANIKUST_QUICK_BENCH_ROUNDS (3).
case_bench() {
  local o wrap n cpuset rounds round mode cap work jobs t0 t1 rc tag peak
  o="$(out bench)"
  rm -rf "$o" && mkdir -p "$o"
  wrap="$W/bench-wrap.sh"
  # The scope's own memory.peak, read by the process that started the build.
  sudo tee "$wrap" >/dev/null <<'EOF'
#!/bin/sh
peak="$1"
shift
"$@"
rc=$?
cg="$(sed -n 's/^0:://p' /proc/self/cgroup)"
cat "/sys/fs/cgroup$cg/memory.peak" >"$peak" 2>/dev/null || echo 0 >"$peak"
exit "$rc"
EOF
  sudo chmod 0755 "$wrap"
  n="$(nproc)"
  [ "$n" -ge 5 ] || fail "the bench needs at least 5 vCPUs, the VM has $n" || return 1
  cpuset="$((n - 4))-$((n - 1))"
  rounds="${RANIKUST_QUICK_BENCH_ROUNDS:-3}"
  [[ $rounds =~ ^[1-9][0-9]?$ ]] || fail "RANIKUST_QUICK_BENCH_ROUNDS must be 1-99" || return 1
  printf 'mode\tcap\twork\tjobs\tround\twall_s\tpeak_mib\trc\n' >"$o/bench.tsv"
  for round in $(seq 0 "$rounds"); do
    for mode in kaniko overlay; do
      for cap in cap4 all; do
        for work in "chain default" "par 1" "par default"; do
          set -- $work
          tag="$mode-$cap-$1-j$2-r$round"
          local scope=(--quiet --scope --collect)
          [ "$cap" = all ] || scope+=(-p CPUQuota=400% -p AllowedCPUs="$cpuset" -p MemoryMax=8G -p MemorySwapMax=0)
          local args=(--context "$SMOKE_CASES_DIR/$CASE" --mode="$mode" --state-dir "$W/bench-$mode"
            --no-push --reproducible --build-arg "BASE=$BASE" --build-arg FILES=20000 --build-arg MB=300
            --metrics-file "$o/$tag.json")
          [ "$1" = chain ] && args+=(-f Dockerfile.chain)
          [ "$2" = default ] || args+=(--jobs "$2")
          rc=0
          t0="$(date +%s%N)"
          sudo systemd-run "${scope[@]}" -- "$wrap" "$o/peak" "$RNK" "${args[@]}" >"$o/$tag.log" 2>&1 || rc=$?
          t1="$(date +%s%N)"
          peak="$(sudo cat "$o/peak" 2>/dev/null || echo 0)"
          printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$mode" "$cap" "$1" "$2" "$round" \
            "$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", (b - a) / 1e9 }')" \
            "$((peak / 1048576))" "$rc" >>"$o/bench.tsv"
          [ "$rc" = 0 ] || { cat "$o/$tag.log" >&2; fail "bench build $tag exited $rc" || return 1; }
        done
      done
    done
  done
  sudo chown -R "$(id -u):$(id -g)" "$o"
  python3 -I "$SMOKE_CASES_DIR/$CASE/bench-summary.py" "$o" >"$o/bench.md" ||
    fail "bench summary" || return 1
  cat "$o/bench.md"
}
