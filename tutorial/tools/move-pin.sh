#!/bin/sh
# Move tutorial/cactup.pin to another commit: `just pin COMMIT`.
#
# Steps 1 to 3 of tutorial/UPDATING.md's procedure, short of the image
# build: lists the commits since the old pin that touched cactup's inputs,
# builds the new cactup (outside the checkout, from the commit's own files)
# and checks every cactup command the tutorial runs against it. Only when
# that check passes does it write the new commit into cactup.pin and rename
# the cactup build the notebooks' prose names (`cactup-` and a commit id).
# It ends by saying what is left: the image build, the checks, the reading.
#
# The build lives in ~/.cache/cactup-tutorial/pin, so a second run (after
# fixing what the check reported) recompiles cactup alone.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
tutorial=$(dirname -- "$here")
repo=$(dirname -- "$tutorial")
pinfile=$tutorial/cactup.pin

[ $# -eq 1 ] || { echo "usage: just pin COMMIT (a commit or branch of this repository)" >&2; exit 2; }
new=$(git -C "$repo" rev-parse --verify --quiet "$1^{commit}") \
    || { echo "move-pin: no commit $1 in this repository (fetch it first)" >&2; exit 1; }
old=$(sed -n 's/^commit[[:space:]]*=[[:space:]]*\([0-9a-f]\{40\}\)[[:space:]]*$/\1/p' "$pinfile")
[ -n "$old" ] || { echo "move-pin: tutorial/cactup.pin names no commit" >&2; exit 1; }
if [ "$new" = "$old" ]; then
    echo "the pin is already $(git -C "$repo" log -1 --format='%h %s' "$new")"
    exit 0
fi
if [ -z "$(git -C "$repo" branch -r --contains "$new")" ]; then
    echo "note: $1 is on no pushed branch yet; push it before committing the pin" >&2
fi

echo "== commits since the old pin that touched cactup's inputs (read each: git show --stat ID)"
git -C "$repo" log --oneline "$old..$new" -- \
    src build.rs Cargo.toml Cargo.lock resources mdb cactup-init.sh cactupdocs
echo

# The commit's cactup sources, as image/build.sh extracts them. Stamped now
# (-m), not with the commit's date: an older commit's files would otherwise
# look older than the last build's, and cargo would keep that build.
cache=${XDG_CACHE_HOME:-$HOME/.cache}/cactup-tutorial/pin
rm -rf "$cache/src"
mkdir -p "$cache/src"
git -C "$repo" archive "$new" -- Cargo.toml Cargo.lock build.rs cactupdocs/Cargo.toml cactupdocs/src \
    src resources mdb cactup-init.sh | tar -x -m -C "$cache/src"
echo "== building cactup at $(git -C "$repo" log -1 --format='%h %s' "$new")"
cargo build --release --locked --quiet --manifest-path "$cache/src/Cargo.toml" --target-dir "$cache/target"
echo

echo "== checking the tutorial's cactup commands against it"
if ! python3 "$here/check-cactup-usage.py" --cactup "$cache/target/release/cactup"; then
    echo
    echo "The pin has not moved. Fix the commands above in the notebooks or the" >&2
    echo "image's scripts, then run 'just pin $1' again." >&2
    exit 1
fi
echo

sed -i "s/^commit[[:space:]]*=.*/commit = $new/" "$pinfile"
echo "== tutorial/cactup.pin: $(git -C "$repo" log -1 --format=%h "$old") -> $(git -C "$repo" log -1 --format=%h "$new")"

# The cactup build the image installs is named for the last commit that
# touched the binary's inputs, not for the pin itself. Keep in sync with
# image/build.sh.
inputs="src build.rs Cargo.toml Cargo.lock resources mdb/GENERATION mdb/generic"
# shellcheck disable=SC2086
old_build=$(git -C "$repo" log -1 --format=%h --abbrev=7 "$old" -- $inputs)
# shellcheck disable=SC2086
new_build=$(git -C "$repo" log -1 --format=%h --abbrev=7 "$new" -- $inputs)
if [ "$old_build" != "$new_build" ]; then
    files=$(grep -rl "cactup-$old_build" "$tutorial/notebooks" || true)
    for f in $files; do
        sed -i "s/cactup-$old_build/cactup-$new_build/g" "$f"
        echo "== ${f#"$repo"/}: cactup-$old_build -> cactup-$new_build"
    done
fi
left=$(grep -rIn --exclude-dir=out --exclude-dir=.venv --exclude-dir=__pycache__ --exclude-dir=node_modules \
    --exclude=UPDATING.md --exclude=SIZING.md -e "$old_build" -e "$(echo "$old" | cut -c1-7)" "$tutorial" || true)
if [ -n "$left" ]; then
    echo
    echo "Still naming the old cactup (check each by hand):"
    printf '%s\n' "$left" | sed "s|^$repo/||"
fi

cat <<EOF

Next (tutorial/UPDATING.md, from step 3):
  just build       watch the bake: lines; "building" rebakes (an hour or more)
  just check-all   then sort any failures
  read the notebooks the commits above touch, add a line to UPDATING.md's
  pin history, and commit it all in one commit
EOF
