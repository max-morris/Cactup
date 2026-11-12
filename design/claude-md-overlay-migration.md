# Draft CLAUDE.md text for overlay migration

Apply to CLAUDE.md's "MDB generation" section when `feature/overlay-migration` merges: replace the "Procedure, all in one commit" paragraph with the first paragraph below, and add the second after it.

**Procedure, all in one commit:** before bumping, snapshot this generation's
overlays with `ci/overlay-fixtures.sh N OUTDIR` run on the pre-bump tree
(into `fixtures/mdb-overlays/gen-N/`; real `machine create` output, never
edited afterward); increment `mdb/GENERATION`; add the overlay migration step
N→N+1 to `mdb::migrate::STEPS` (a step that only raises the number is still a
step: `raise`); append `## Generation N+1` to `mdb/GENERATIONS.md` saying
what changed, an "Automatically:" paragraph on what the migration step does
and when it stops, and the step-by-step hand recipe for what it refuses and
what is optional (users follow it, and the refusal points at it); migrate
every in-repo machine. Tests fail unless every generation up to N+1 has a
`GENERATIONS.md` entry (`every_generation_is_documented`), every older one a
step (`every_generation_has_a_migration`) and a snapshot
(`every_older_generation_has_snapshots`), and every snapshot and edge case
does what it should (`every_snapshot_migrates`): generation-1 snapshots and
the refusal cases are refused untouched, the rest migrate, load, and migrate
to nothing the second time. A step's checks read the files as its own
generation wrote them, leniently, never through today's `Meta`: later
generations change that schema. The `mdb-compat` CI job
(`ci/mdb-generation-guard.sh`) loads every machine in both directions
against that same commit (the last one to raise `mdb/GENERATION` to its
current number) and tells you to bump if either fails. It checks *loading*
only: template variables and the `.py` protocol still need a human.
(Optionlist header keys need no judgment since generation 2: the header is
closed, so a new key fails to load in an older binary and the guard catches
it.) User docs: `cactupdocs/content/authors/mdb-generations.md`.

**Overlay migration (`src/mdb/migrate.rs`).** A user-layer machine older
than the binary is brought forward on disk the first time a login command
loads it: under a `LinkLock` beside the overlay, the steps run in memory and
the result must pass today's parse and validation; only then is a full copy
made in `<CACTUP_HOME>/machine-backups/<name>/generation-<N>/` (kept forever,
with a `NOTES.txt`; `.committing`/`.complete` tell an interrupted run from a
finished one) and the changed files written, `meta.toml` last (its stamp is
the commit point). A refusal writes nothing, not even the copy; a lock or
I/O failure is transient ("run the command again"). A step preserves
behavior or refuses: it applies mechanical edits (through `toml_edit`, so
comments survive), notes a difference that can only surface as a loud error,
and stops (keeping the steps already done) where the machine could quietly
act differently. It never applies an optional recommendation, never edits a
script, and never writes a key the target generation's first binary lacks.
Generation 1 is never migrated: the 1→2 step always refuses and the user
follows the hand recipe. A refusal is short: the machine and file, its
generation and the binary's, what stopped the step, and the next step on a
line of its own (the generation's own section of `GENERATIONS.md`, or moving
the overlay to the hidden `machines/.<name>.old`, or `.<name>.old.2` and so
on when that is taken, and re-creating it). An
overlay newer than the binary is refused, never downgraded; one with no
generation is assumed current and never rewritten. Hidden directories are not
machines (`machine create` rejects a name starting with '.'). Migration never
runs on the compute path (D11), and there is no `cactup machine migrate`.

Also adjust the "Relation to \"no backward compatibility\"" paragraph that follows: its "no migrations" means the published MDB itself (bump instead); overlays are now migrated forward by the binary. Say so in one clause.
