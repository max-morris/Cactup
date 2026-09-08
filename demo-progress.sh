#!/usr/bin/env bash
# Live demo of the component-fetch progress UX, driving the REAL cactup
# binary against a sandboxed installation fabricated under /tmp — your actual
# ~/.cactup is never touched (HOME is overridden for the cactup process).
#
#   ./demo-progress.sh           # both acts: a fresh clone, then a refetch
#   ./demo-progress.sh clone     # act 1 only (wipes and re-creates the sandbox)
#   ./demo-progress.sh refetch   # act 2 only (reuses the sandbox from act 1)
#
# NEEDS A REAL TERMINAL. The progress bars are drawn only when stderr is a
# tty: piped into a file or a pager you get the history lines and nothing
# else, which is deliberate (job logs stay readable) but makes for a poor
# demo. Don't redirect it.
#
# ACT 1 — a fresh clone of six components (four repos, two downloads), so
# you can watch:
#   * one line per component, no matter how deep gix's own progress tree goes
#   * the phase label and the bar tracking whatever is *moving*: the server's
#     "Counting/Compressing objects", then "receiving pack" (bytes off the
#     wire), "indexing pack", "checking out" (files into the worktree)
#   * the name column holding still while the phases change underneath, and
#     the component name louder than the phase it is in
#   * a green line per finished component, landing in the scrollback above
#     the bars still running — in the same columns
#   * a red line for the repo whose host does not resolve, and for the
#     download that 404s, while everything else carries on
#
# ACT 2 — a refetch over act 1's tree, with one repo dirtied behind cactup's
# back, so you can watch what a *typical* refetch looks like:
#   * silence from every component already at the wanted commit — the point
#     being that eighty "already up to date" lines would bury the news
#   * one yellow line for the dirtied repo, because -f overwrote local work:
#     the fetch changed something you did not ask it to, and says so
#
# The network repos are real (gitoxide is a ~64MB pack — that is the one that
# shows every phase properly); cactup clones itself from disk, which is
# instant and lands in the scrollback while the rest are still negotiating.

set -euo pipefail

MODE="${1:-both}"
REPO="$(cd "$(dirname "$0")" && pwd)"
BIN="$REPO/target/debug/cactup"
ROOT=/tmp/cactup-progress-demo
HOME_DIR="$ROOT/home"
INST="$ROOT/inst"
LIST="$ROOT/demo.th"

case "$MODE" in
  both | clone | refetch) ;;
  *) echo "usage: $0 [both | clone | refetch]" >&2; exit 2 ;;
esac

if [[ ! -t 2 ]]; then
  echo "note: stderr is not a terminal, so no bars will be drawn — see the" >&2
  echo "      header of $0. Run it in a terminal without redirecting." >&2
  echo >&2
fi

echo "==> Building cactup…" >&2
cargo build --quiet --manifest-path "$REPO/Cargo.toml"

fabricate() {
  echo "==> Fabricating a sandboxed installation at $ROOT" >&2
  rm -rf "$ROOT"
  # `cactup install` would create the source tree; a hand-fabricated
  # installation has to, or refetch's final write of the live thornlist has
  # nowhere to land.
  mkdir -p "$HOME_DIR/.cactup" "$INST/.cactup" "$INST/Cactus/thornlists"

  cat > "$HOME_DIR/.cactup/database.json" <<EOF
{
  "schema": 1,
  "cactup-version": "0.1.0",
  "installations": {
    "demo": { "alias": "demo", "release": null, "path": "$INST" }
  },
  "active-installation": "demo"
}
EOF

  cat > "$INST/.cactup/installation.toml" <<'EOF'
schema = 1
root-dir = "Cactus"
EOF

  # Names of deliberately different widths — the name column is sized to the
  # longest of them, and every phase, number and bar starts after it.
  cat > "$LIST" <<EOF
!CRL_VERSION = 1.0
!DEFINE ROOT = Cactus

# A ~64MB pack: the one that shows every phase for long enough to read.
!TARGET   = \$ROOT/arrangements
!TYPE     = git
!URL      = https://github.com/GitoxideLabs/gitoxide
!NAME     = gitoxide
!CHECKOUT = gitoxide/gix

# Small and quick: finishes while the big ones are still going, so its green
# line lands in the scrollback above bars that are still moving.
!TARGET   = \$ROOT/arrangements
!TYPE     = git
!URL      = https://github.com/Byron/prodash
!NAME     = prodash
!CHECKOUT = prodash/src

# Local, so it is done almost before the renderer starts. Act 2 dirties this
# one — a local clone keeps the second act quick.
!TARGET   = \$ROOT/arrangements
!TYPE     = git
!URL      = $REPO
!NAME     = cactup
!CHECKOUT = cactup/src

# Never resolves: one red line, and the rest of the fetch carries on.
!TARGET   = \$ROOT/arrangements
!TYPE     = git
!URL      = https://github.invalid/nothing/here
!NAME     = unreachable-remote
!CHECKOUT = unreachable/src

# ~11MB with a Content-Length, so this bar fills and shows a percentage —
# the pack downloads above cannot, as git announces no pack size.
#
# kernel.org rather than ftp.gnu.org for no deep reason — cactup identifies
# itself now (see download.rs::USER_AGENT), which is what ftp.gnu.org was
# 403ing — this mirror is simply the faster of the two.
!TARGET   = \$ROOT/downloads
!TYPE     = https
!URL      = https://mirrors.kernel.org/gnu/bash
!CHECKOUT = bash-5.2.tar.gz

# 404: the second red line.
!TARGET   = \$ROOT/downloads
!TYPE     = https
!URL      = https://mirrors.kernel.org/gnu/bash
!CHECKOUT = bash-0.0.tar.gz
EOF
}

act1() {
  fabricate
  echo
  echo "==> ACT 1:  cactup inst refetch demo.th        (a fresh clone of everything)"
  echo
  echo "    Watch:  one line per component — never a nested bar, however deep"
  echo "            gix reports underneath it"
  echo "            the label and bar following whatever is MOVING:"
  echo "              Counting/Compressing objects  (the server, with a %)"
  echo "              receiving pack                (bytes; no % — git sends no size)"
  echo "              indexing pack"
  echo "              checking out                  (files, with a %)"
  echo "            the name column holding still while those change under it,"
  echo "            and the name brighter than the phase"
  echo "            green lines landing above the running bars, same columns"
  echo "            red for unreachable-remote and the 404'd download"
  echo "    Also:   Ctrl-C once — it stops within moments and says what it"
  echo "            did not fetch (nothing masquerades as complete)"
  echo
  read -rp "    Press Enter to start…"
  echo
  HOME="$HOME_DIR" "$BIN" inst refetch "$LIST" || true
}

act2() {
  if [[ ! -d "$INST/Cactus/repos/cactup" ]]; then
    echo "==> No sandbox from act 1 — running it first." >&2
    act1
  fi
  echo
  echo "==> Dirtying one repo behind cactup's back…"
  echo "// a hand edit cactup never made" >> "$INST/Cactus/repos/cactup/src/main.rs"
  echo "    $INST/Cactus/repos/cactup/src/main.rs"
  echo
  echo "==> ACT 2:  cactup inst refetch demo.th -f     (a typical refetch)"
  echo
  echo "    Watch:  SILENCE from every component already at the wanted commit."
  echo "            That is the point: eighty 'already up to date' lines would"
  echo "            bury the news. The summary still counts them."
  echo "            one YELLOW line for cactup — -f overwrote a local edit, so"
  echo "            the fetch changed something you did not ask for, and says so"
  echo "            (the overwritten file is kept under refetch-backups/)"
  echo
  read -rp "    Press Enter to start…"
  echo
  HOME="$HOME_DIR" "$BIN" inst refetch "$LIST" -f || true
}

case "$MODE" in
  clone) act1 ;;
  refetch) act2 ;;
  both)
    act1
    act2
    ;;
esac

echo
echo "Demo over. Also try:  $0 clone   |   $0 refetch"
echo "  (and once more with a pipe — 'demo-progress.sh clone 2>&1 | cat' — to see"
echo "   the same run reduced to just its history lines, as a job log gets it)"
echo "Sandbox: $ROOT (safe to rm -rf)"
