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
- `@ENV(NAME)@` tokens in MDB templates and paths read arbitrary environment variables at use time — see [Scripts & Variables](../authors/scripts-and-variables.html).

## Directory layout

- `~/.cactup/` — cactup's root: metadata, database, and caches.
- `~/.cactup/mdb/` — the system machine database (git-managed by cactup).
- `~/.cactup/machines/` — your own machine definitions (the user overlay).
- Install prefix (default `~/.cactup/cacti/`) — Cactus source trees and builds; override with `--install-prefix`.
- `simulation-home` / `test-home` — simulation and test-suite output roots, set per machine (fallbacks `~/.cactup/simulations/` and `~/.cactup/tests/`).
