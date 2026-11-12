#!/bin/bash
# Regenerate the overlay snapshot for one MDB generation.
#
#   ci/overlay-fixtures.sh N OUTDIR
#
# Run it with the current directory inside a tree whose mdb/GENERATION is N
# (the PRE-bump tree: the last commit before the bump to N+1, or the commit
# that raised it to N). The script itself lives in the newest tree; call it
# by path from the old tree:
#
#   cd /path/to/old-tree && bash /path/to/new-tree/ci/overlay-fixtures.sh 3 /out
#
# It builds that tree's cactup, and in a scratch CACTUP_HOME creates a user-MDB
# overlay for each machine below with that binary's own
# `machine create <m> --from-existing <m> --silent --no-discover`. Then it
# scrubs what depends on the machine it ran on, and copies the overlay
# directories to OUTDIR/<machine>/. Result: exactly what a user of that
# generation's binary has on disk.
#
# Isolation: CACTUP_HOME is a scratch dir (never ~/.cactup); `--mdb-path`
# points the binary at the tree's own mdb/, so it never syncs the MDB from the
# network; `-K autoupdate=off` turns auto-update off (every generation's
# binary accepts it); `--hostname` is fixed so the machine's nickname and
# hostname come out the same on every run.
#
# Scrub (the only post-processing):
#   [hardware] max-cpus-per-node = 16, memory = 65536, max-gpus-per-node = 0
#       (only the keys the file has; machine create overwrites them with
#       what it detects on the host)
#   the scratch home, $HOME and the real user and host names in any text
#       file -> fixed values (install-home, simulation-home, anything else)
#   __pycache__ directories are deleted.
# The script fails if anything of the real host still shows after the scrub.
#
# Quirks of the old binaries: none needed hand steps for gens 1-4. `rostam`
# is not in the MDB before generation 3 (17ef80f), so it is skipped for 1-2. Cargo needs the network the first time (crates, git deps).
set -euo pipefail

gen=${1:?usage: overlay-fixtures.sh N OUTDIR}
out=${2:?usage: overlay-fixtures.sh N OUTDIR}
tree=$(git rev-parse --show-toplevel)
[ -f "$tree/mdb/GENERATION" ] || { echo "no mdb/GENERATION in $tree" >&2; exit 1; }
have=$(tr -d '[:space:]' < "$tree/mdb/GENERATION")
if [ "$have" != "$gen" ]; then
    echo "tree $tree is at MDB generation $have, not $gen" >&2
    exit 1
fi

machines=(generic qbc.loni.org qbd db1.hpc.lsu.edu mel5)
if [ "$gen" -ge 3 ]; then machines+=(rostam); fi

scratch_root=${FIXTURE_SCRATCH:-$HOME/tmp/overlay-fixtures}
home=$scratch_root/home-gen$gen
case $home in
    "$HOME"/.cactup*|"$HOME") echo "refusing scratch home $home" >&2; exit 1 ;;
esac
rm -rf "$home"
mkdir -p "$home"
export CACTUP_HOME=$home

# One target dir per generation: trees of different generations share package
# names and workspace-relative paths, so cargo would treat a shared target dir
# as fresh and hand back another tree's binary. The binary's path is read from
# cargo's own report.
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$tree/target}/gen$gen
bin=$(cd "$tree" && cargo build --locked --bin cactup --message-format=json-render-diagnostics \
    | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -n 1)
[ -n "$bin" ] && [ -x "$bin" ] || { echo "cargo built no cactup binary" >&2; exit 1; }

for m in "${machines[@]}"; do
    [ -d "$tree/mdb/$m" ] || { echo "machine $m is not in $tree/mdb" >&2; exit 1; }
    "$bin" --mdb-path "$tree/mdb" --hostname fixture.example.org -K autoupdate=off \
        machine create "$m" --from-existing "$m" --silent --no-discover
done

dest=$out
rm -rf "$dest"
mkdir -p "$dest"
user=$(id -un)
realhost=$(hostname)
realshort=${realhost%%.*}
for m in "${machines[@]}"; do
    src=$home/machines/$m
    [ -f "$src/meta.toml" ] || { echo "no overlay for $m" >&2; exit 1; }
    cp -r "$src" "$dest/$m"
    find "$dest/$m" -name __pycache__ -type d -prune -exec rm -rf {} +
    # Hardware: only inside [hardware] (queue tables carry real MDB values).
    awk '
        /^\[/ { inhw = ($0 == "[hardware]") }
        inhw && /^max-cpus-per-node[ ]*=/ { print "max-cpus-per-node = 16"; next }
        inhw && /^memory[ ]*=/            { print "memory = 65536"; next }
        inhw && /^max-gpus-per-node[ ]*=/ { print "max-gpus-per-node = 0"; next }
        { print }
    ' "$dest/$m/meta.toml" > "$dest/$m/meta.toml.new"
    mv "$dest/$m/meta.toml.new" "$dest/$m/meta.toml"
    # Paths and names of the machine that ran this.
    find "$dest/$m" -type f | while read -r f; do
        if grep -Iq . "$f"; then
            sed -i \
                -e "s#$home#/scratch/fixture-home#g" \
                -e "s#$HOME#/home/fixture#g" \
                -e "s#\\b$realshort\\.[A-Za-z0-9.-]*#fixture.example.org#g" \
                "$f"
        fi
    done
done

# Leak check: nothing of the real host may remain.
leaks=$(grep -rIn -e "$home" -e "$HOME" -e "$realhost" "$dest" || true)
leaks+=$(grep -rInw -e "$realshort" "$dest" || true)
leaks+=$(grep -rInE "(^|[^-A-Za-z0-9_@])$user([^-A-Za-z0-9_@(]|$)" "$dest" || true)
if [ -n "$leaks" ]; then
    echo "scrub left personal data behind:" >&2
    echo "$leaks" >&2
    exit 1
fi
echo "wrote ${#machines[@]} overlays for generation $gen to $dest"
