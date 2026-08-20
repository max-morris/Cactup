#!/usr/bin/env bash
# Live demo of the new `sim log` follow UX, driving the REAL cactup binary
# against a sandboxed installation fabricated under /tmp — your actual
# ~/.cactup is never touched (HOME is overridden for the cactup process).
#
#   ./demo-follow.sh          # -f: the new side-by-side TUI (default)
#   ./demo-follow.sh -o       # stream stdout only (pipe-friendly)
#   ./demo-follow.sh -e       # stream stderr only
#   ./demo-follow.sh static   # no flag: plain tail of both, then exit
#
# A background writer keeps appending Einstein-Toolkit-flavored output to the
# fake simulation's .out/.err the whole time: steady evolution iterations,
# periodic horizon finds and checkpoints, occasional stderr warnings, a
# regridding burst every ~100 iterations (shows off the 50ms follow latency),
# and the odd ANSI-colored / tab-ridden line (shows off TUI sanitization).
# The -f side-by-side TUI also has vim-style line selection + copy (`v`,
# `y`/`Y`, OSC 52) and per-pane regex search (`/`, `?`, `n`/`N`) — the "Try:"
# list below points at real matches this writer produces (checkpoint dumps,
# the periodic Dissipation warning) to poke at both.
# The writer dies automatically when you quit (q / Esc / Ctrl-C).

set -euo pipefail

MODE="${1:--f}"
REPO="$(cd "$(dirname "$0")" && pwd)"
BIN="$REPO/target/debug/cactup"
ROOT=/tmp/cactup-follow-demo
SIM=gw150914
INST="$ROOT/inst"
SIMDIR="$INST/simulations/$SIM"
RDIR="$SIMDIR/output-0000"
OUT="$RDIR/$SIM.out"
ERR="$RDIR/$SIM.err"

echo "==> Building cactup…" >&2
cargo build --quiet --manifest-path "$REPO/Cargo.toml"

echo "==> Fabricating a sandboxed installation at $ROOT" >&2
rm -rf "$ROOT"
mkdir -p "$ROOT/home/.cactup" "$INST/.cactup" "$SIMDIR/.cactup" "$RDIR"
ln -s output-0000 "$SIMDIR/output-0000-active"

cat > "$ROOT/home/.cactup/database.json" <<EOF
{
  "schema": 1,
  "cactup-version": "0.1.0",
  "installations": {
    "demo": { "alias": "demo", "release": null, "path": "$INST" }
  },
  "active-installation": "demo"
}
EOF

cat > "$INST/.cactup/simulations.toml" <<EOF
schema = 1

[simulations.$SIM]
dir = "$SIMDIR"
config = "et-demo"
created = "2026-08-14T12:00:00Z"
EOF

cat > "$SIMDIR/.cactup/simulation.toml" <<EOF
schema = 1
machine = "demo-cluster"
simulation-id = "simulation-$SIM-demo-cluster-localhost-$USER-20260814-120000-4242"
sourcedir = "$INST/Cactus"
configuration = "et-demo"
config-id = "config-demo"
build-id = "build-demo"
executable = "$SIMDIR/.cactup/exe"
optionlist = "demo.cfg"
parfile = "$SIM.par"
alias = "demo"
EOF

# --- Seed history, so panes/tails have something to show at startup --------
{
  echo "--------------------------------------------------------------------------------"
  echo "  Cactus 4.16   —   config et-demo   —   simulation $SIM"
  echo "  2 levels of mesh refinement, 40 MPI ranks on demo-cluster"
  echo "--------------------------------------------------------------------------------"
  for i in $(seq 1 120); do
    t=$(awk "BEGIN{printf \"%.3f\", $i*0.03125}")
    echo "INFO (Carpet): it $i  t=$t  |H|=2.1e-09  lapse_min=0.412  speed=141 it/h"
  done
} > "$OUT"
{
  echo "WARNING[L2] (NaNChecker): checking 8 grid variables every 16 iterations"
  echo "INFO (MPI): rank layout 8x5, ghost zones 3"
} > "$ERR"

# --- The background writer -------------------------------------------------
writer() {
  local i=120
  while :; do
    i=$((i + 1))
    local t
    t=$(awk "BEGIN{printf \"%.3f\", $i*0.03125}")
    echo "INFO (Carpet): it $i  t=$t  |H|=$(awk "BEGIN{printf \"%.1e\", 2e-9+($i%7)*3e-10}")  lapse_min=0.$((400 + i % 37))  speed=$((138 + i % 9)) it/h" >> "$OUT"

    if (( i % 25 == 0 )); then
      {
        echo "INFO (CarpetIOHDF5): dumping checkpoint at it $i"
        printf 'INFO (CarpetIOHDF5):\tlevel\tpoints\tbytes\n'
        printf 'INFO (CarpetIOHDF5):\t0\t129^3\t68.2M\n'
        printf 'INFO (CarpetIOHDF5):\t1\t97^3\t29.0M\n'
        echo "INFO (CarpetIOHDF5): checkpoint written: checkpoint.chkpt.it_$i.h5"
      } >> "$OUT"
    fi
    if (( i % 40 == 0 )); then
      {
        echo "INFO (AHFinderDirect): searching for horizon 1/2 at t=$t"
        printf 'INFO (AHFinderDirect): \033[32mfound horizon 1\033[0m: area 197.39, mass 0.4527\n'
        echo "INFO (AHFinderDirect): found horizon 2: area 196.88, mass 0.4521"
      } >> "$OUT"
    fi
    if (( i % 100 == 0 )); then
      # Regridding burst: ~20 lines as fast as the fs will take them —
      # watch them land near-instantly in the pane (50ms floor).
      for lev in $(seq 0 19); do
        echo "INFO (CarpetRegrid2): level $((lev % 5)): regridding box $((lev / 5)) around puncture $((lev % 2))" >> "$OUT"
      done
    fi
    if (( i % 17 == 0 )); then
      echo "WARNING[L1] (NaNChecker): it $i: 0 NaNs in 8 variables (largest |gxx| 1.87)" >> "$ERR"
    fi
    if (( i % 61 == 0 )); then
      echo "WARNING[L2] (Dissipation): order lowered near refinement boundary at it $i" >> "$ERR"
    fi
    sleep 0.2
  done
}
writer >/tmp/cactup-follow-demo-writer.log 2>&1 &
WRITER_PID=$!
trap 'kill "$WRITER_PID" 2>/dev/null || true' EXIT

# --- Run the real thing ----------------------------------------------------
echo
case "$MODE" in
  -f)
    echo "==> Launching:  cactup sim log $SIM -f      (side-by-side TUI)"
    echo
    echo "    Try:  scroll up (wheel or ↑/PgUp)  — pane pauses, ⏸ +N counts unseen lines"
    echo "          End (or scroll to bottom)    — pane snaps back to ● live"
    echo "          Tab / click                  — switch pane focus (cyan border)"
    echo "          ←/→                          — pan the long checkpoint lines"
    echo "          q                            — quit (writer is cleaned up)"
    echo "          v then j/k/G, y              — select lines in the focused pane, y copies them (OSC 52)"
    echo "          y  /  Y                      — y copies the view; Y copies the whole 10k-line pane"
    echo "          /checkpoint  then n/N        — search stdout, step through every checkpoint dump"
    echo "          ?Dissipation                 — search stderr backwards for the order-lowered warning"
    echo "          m                            — free the mouse — drag-select with your terminal instead"
    echo
    read -rp "    Press Enter to start…"
    HOME="$ROOT/home" "$BIN" sim log "$SIM" -f
    ;;
  -o|--follow-out) HOME="$ROOT/home" "$BIN" sim log "$SIM" -o ;;
  -e|--follow-err) HOME="$ROOT/home" "$BIN" sim log "$SIM" -e ;;
  static)          HOME="$ROOT/home" "$BIN" sim log "$SIM" ;;
  *) echo "usage: $0 [-f | -o | -e | static]" >&2; exit 2 ;;
esac

echo
echo "Demo over. Also try:  $0 -o   |   $0 -e   |   $0 static"
echo "  ($0 -o | grep -c Carpet   — single-stream modes keep stdout pipe-clean)"
echo "Sandbox: $ROOT (safe to rm -rf)"
