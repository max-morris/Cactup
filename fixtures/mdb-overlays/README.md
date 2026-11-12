# User-MDB overlay fixtures

Test inputs for migrating user-MDB overlays (`~/.cactup/machines/<name>/`)
forward across MDB generations (see `design/mdb-overlay-migration-plan.md`).

## `gen-N/<machine>/`: snapshots

Real `cactup machine create <m> --from-existing <m> --silent --no-discover`
output of the binary built from the commit that last raised `mdb/GENERATION`
to N, run against that same commit's own `mdb/`:

| N | commit    | machines |
|---|-----------|----------|
| 1 | `38e118a` | generic, qbc.loni.org, qbd, db1.hpc.lsu.edu, mel5 |
| 2 | `ec7ec7a` | the same five |
| 3 | `17ef80f` | the five plus rostam |
| 4 | `13928d4` | the six |

`rostam` is not in the MDB before generation 3, so gens 1 and 2 lack it.

**A committed `gen-N` directory is never edited.** It records what that
generation's binary wrote, which is what users have on disk. A bump to
generation N+1 adds `gen-N` by running the script on the pre-bump tree; it
never touches the older ones.

### Regenerating

From a checkout of the commit (a detached worktree is fine), with the script
taken from any tree that has it:

    cd /path/to/tree-at-generation-N
    bash /path/to/ci/overlay-fixtures.sh N /path/to/fixtures/mdb-overlays/gen-N

The script builds that tree's cactup, creates the overlays in a scratch
`CACTUP_HOME` (never `~/.cactup`), points the binary at the tree's own `mdb/`
(`--mdb-path`, no network sync), turns auto-update off, scrubs, and copies.
Set `FIXTURE_SCRATCH` to move the scratch directory (default
`~/tmp/overlay-fixtures`), and `CARGO_TARGET_DIR` for the build output.

### Scrubbed values

`machine create` records what it detects on the host. The script replaces:

- `[hardware]`: `max-cpus-per-node = 16`, `memory = 65536`, and
  `max-gpus-per-node = 0` where the file has it. Queue tables are untouched.
- The scratch home becomes `/scratch/fixture-home` (so `install-home` and
  `simulation-home` read `/scratch/fixture-home/cacti` and
  `/scratch/fixture-home/simulations`); `$HOME` becomes `/home/fixture`; the
  real host name becomes `fixture.example.org`.
- `__pycache__` directories are deleted.

The hostname given to `machine create` is `fixture.example.org`, so every
overlay has `hostname = "fixture.example.org"` and `nickname = "fixture"`.
The script fails if the real home, host name or user name still appears.

## `edge/<case>/`: hand-edited cases

Each is a snapshot with only what the case needs changed, plus `expect.toml`:

- `result-generation`: the overlay's generation after the load (`0`: none
  recorded).
- `outcome`: `"migrated"`, `"refused"`, or `"untouched"` (not migrated, loads
  with a warning).
- `findings`: short strings naming the notes or refusal kinds expected.

| case | based on | expected |
|------|----------|----------|
| `gen1-overlay` | gen-1 generic | refused: generation 1 is never migrated |
| `gen2-py-env-setup-universes` | gen-2 qbc.loni.org | refused: a `.py` build-submit script reads `ENV_SETUP`, and the submit-phase env differs between build universe `et-sing` and route universe `plain` |
| `gen2-both-key-names` | gen-2 generic | refused: `env-build-setup` and `build-env-setup` in one table (a gen-2 binary cannot load it either) |
| `gen2-provisioning-lines` | gen-2 generic | migrated, note "may belong in provision" (`mkdir`, `pip install`) |
| `gen2-comments` | gen-2 generic | migrated, comments everywhere survive |
| `gen4-sh-allocation` | gen-4 generic | migrated, number only: a `.sh` script with `#SBATCH -A @ALLOCATION@` is untouched |
| `newer-than-binary` | gen-4 generic | refused: `mdb-generation = 99`, files untouched |
| `no-generation` | gen-4 generic | untouched: no `mdb-generation`, warning only |

Checked against the old binaries: the gen-2 cases other than
`gen2-both-key-names` load with the gen-2 binary and `gen4-sh-allocation` with
the gen-4 binary. `gen2-both-key-names` is rejected by it (unknown field), as
intended. With the gen-4 binary, `gen1-overlay` and `newer-than-binary` give
the old generation errors, and `no-generation` loads with the "records no MDB
generation" warning.
