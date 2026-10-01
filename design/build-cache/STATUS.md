# Build cache: status

Read this first when picking the work up. The plan is `PLAN.md` in this
directory; the interface contract with the build-speed work is
`~/tmp/build-cache-speedup-contract/CONTRACT.md`.

## Rules that are easy to forget

- Work only in this worktree (`.claude/worktrees/build-cache`, branch
  `feature/build-cache`). master is not touched.
- Test builds, benchmarks, and anything that writes into a Cactus tree go in
  the install `~/cacti/build-cache` (alias `build-cache`), plus one sibling
  alias `build-cache-b` for cross-installation measurements.
  `/home/max/Cactus-2026` is read-only reference. Never write in
  `~/cacti/speedup-build`.
- Every milestone ends at a review gate: two independent harsh reviewer
  agents, same brief, full milestone diff. Fix or answer every blocking
  finding and re-review until both sign off in the same round. Record the
  verdicts below.
- Re-read the contract file at every milestone and keep the cache-side
  sections current.
- Commits carry no AI attribution. Never run `cargo fmt`.

## Milestones

| Milestone | Scope | State |
|---|---|---|
| M0a | Wrapper dispatch, fail-open paths, panic hook, probe and `inject.mk`, per-build config, knobs | in progress |
| M0b | Argument parser, platform/identity/environment digests, key, `events.log`, `cache report` | not started |
| M0c | Measurements in `~/cacti/build-cache`, written results | not started |
| M1a | Store: publish, restore, invalidate | not started |
| M1b | Serving, double check, audit mode, two-installation audit build | not started |
| M1c | `cache stats/gc/verify`, size notice, spec section, contract in `CLAUDE.md`, user docs | not started |

## Log

- 2026-10-01: Plan approved. Worktree and branch created from master
  `47c67f5`. Contract file written; the build-speed session
  (`speedup-build-d4`) was not reachable by direct message, so coordination
  runs through the contract file. Questions a-d in the contract are open.

## Review verdicts

(none yet)

## Next step

M0a: add `src/objcache/` with the wrapper entry point and its dispatch at the
top of `main`, then the probe verb and `inject.mk` writer, then the
`prepare` hook and knobs.
