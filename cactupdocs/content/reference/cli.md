+++
title = "CLI Reference"
description = "Complete cactup command-line interface documentation"
+++

# cactup Command-Line Interface

The reference below is **generated directly from cactup's command definitions**, so it always matches the version you have installed. For task-oriented guides, start with the [User Guide](../users/getting-started.html).

## Full command reference

Global options (available on every subcommand) are listed first, followed by every command with its arguments and flags.

{{cactup:cli}}

## Exit codes

- `0` — Success
- `1` — General error (a command failed at runtime)
- `2` — CLI argument error (invalid flags or arguments)

## Environment variables

- `HOME` — Locates cactup's root directory (`~/.cactup`).
- Scheduler variables (e.g. `SLURM_JOB_ID`, `PBS_JOBID`) — how cactup detects whether it is running inside a job allocation. Which variables matter is declared per machine via `[scheduler].allocation-env`.
- `@ENV(NAME)@` tokens in MDB templates, parfiles and paths read arbitrary environment variables at use time; `@ENV-OPTIONAL(NAME)@` and `@ENV-OPTIONAL(NAME, default)@` tolerate an unset one — see [Scripts & Variables](../authors/scripts-and-variables.html#substitution-rules).

## Knobs and `-K`

Knobs are per-machine default values stored in `~/.cactup/database.json` and
managed with `cactup knob` (standard knobs such as `allocation` and `queue`, plus
user-defined custom knobs created with `-c`). The global `-K NAME=VALUE` flag —
also spelled `-K NAME VALUE`, repeatable, accepted before or after the
subcommand — overlays a knob for one command without storing it. Templates read
knobs with `@KNOB(name)@`; see [Running Simulations](../users/running-simulations.html#knobs).

## Directory layout

- `~/.cactup/` — cactup's root: metadata, database, and caches.
- `~/.cactup/mdb/` — the system machine database (git-managed by cactup).
- `~/.cactup/machines/` — your own machine definitions (the user overlay).
- `~/.cactup/refetch-backups/<alias>/<timestamp>/` — files backed up before
  `cactup installation refetch --overwrite-modified` (or `-f`, or a targeted
  `--overwrite <name>`) fetches over a repo with local changes.
- Install prefix (default `~/.cactup/cacti/`) — Cactus source trees and builds; override with `--install-prefix`.
- `<installation root>/.cactup/fetch-state.toml` — per-repo record of what
  `cactup` itself fetched into an installation; `refetch --prune` only ever
  removes repos recorded here.
- `<installation root>/.cactup/thornlists/` — timestamped snapshots of an
  installation's live thornlist, taken before `refetch --replace-thornlist`
  (or `-f`) overwrites it.
- `<installation root>/Cactus/repos/` — the fetched component repos backing
  the arrangement symlinks under `Cactus/arrangements/`. `cactup installation
  delta` reports how these differ from the last fetch; `cactup config delta`
  reports how they differ from a config's last build.
- `<installation root>/Cactus/configs/<name>/cactup-config.toml` — a config's
  build metadata, including the per-repo source state it was built from, which
  is what lets a later `cactup build` notice edited or refetched sources.
- `simulation-home` / `test-home` — simulation and test-suite output roots, set per machine (fallbacks `~/.cactup/simulations/` and `~/.cactup/tests/`).
