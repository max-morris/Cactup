# Build cache: status

Read this first when picking the work up. The plan is `PLAN.md` in this
directory; the interface contract with the build-speed work is
`~/tmp/build-cache-speedup-contract/CONTRACT.md`; the spec is §18 of
`design/cactup-simfactory-design-new.md`.

## Rules that are easy to forget

- Work only in this worktree (`.claude/worktrees/build-cache`, branch
  `feature/build-cache`). master is not touched.
- Test builds, benchmarks, and anything that writes into a Cactus tree go in
  the install `~/cacti/build-cache` (alias `build-cache`), plus one sibling
  alias `build-cache-b` for cross-installation measurements.
  `/home/max/Cactus-2026` is read-only reference. Never write in
  `~/cacti/speedup-build`.
- Turn the cache on per command (`cactup -K build-cache=record build …`),
  never with `cactup knob`: the instance's database is shared with other
  sessions and with installed cactup builds that do not know the knob.
- Every milestone ends at a review gate: two independent harsh reviewer
  agents, same brief, full milestone diff. Fix or answer every blocking
  finding and re-review until both sign off in the same round. Record the
  verdicts below.
- Re-read the contract file at every milestone and keep the cache-side
  sections current.
- `CLAUDE.md` is untracked and shared by every session in the repository:
  do not edit it from this branch. Its text waits in `CLAUDE-contract.md`.
- Commits carry no AI attribution. Never run `cargo fmt`.

## Milestones

| Milestone | Scope | State |
|---|---|---|
| M0a | Wrapper dispatch, fail-open paths, panic hook, probe and `inject.mk`, per-build config, knobs | implemented; in review |
| M0b | Argument parser, platform/identity/environment digests, key, `events.jsonl`, `cache report` | not started |
| M0c | Measurements in `~/cacti/build-cache`, written results | not started |
| M1a | Store: publish, restore, invalidate | not started |
| M1b | Serving, double check, audit mode, two-installation audit build | not started |
| M1c | `cache stats/gc/verify`, size notice, contract into `CLAUDE.md`, user docs | not started |

## What M0a is

- `src/objcache/mod.rs`: knobs (`build-cache`, `build-cache-dir`), the
  frozen per-build `BuildConf` (`<attempt>/cc/config.toml`), `stage` (called
  from `prepare`), and the two build-script steps.
- `src/objcache/probe.rs`: `cactup __cc-probe`, the `inject.mk` writer, the
  make self-test.
- `src/objcache/wrapper.rs`: `cactup __cc` / `cactup-cc`, dispatched first
  thing in `main`; pass-through, record mode, signal forwarding, panic hook.
- `tests/objcache.rs`: the binary driven as make drives it, including one
  test under real `make`.
- Design decisions made while building it, beyond the plan:
  - The wrapper is told which make variable it stands in for (`CC:1`), and
    honors a thorn that reassigns the compiler after the fragment is read
    (it finds the exported global in its environment). No thorn in the
    current Einstein Toolkit does this, but replacing a thorn's compiler
    silently would be a change in what gets built.
  - `rustix` is a direct dependency, for `kill(2)` and `waitpid(2)` only.
- Smoke build (2026-10-01): `~/cacti/build-cache/smoke.th` (25 thorns: C,
  C++, F77, F90), config `smoke`, machine `plato`, GNU make 4.4.1, GCC 14.2.
  357 events for 357 objects; every object byte-identical to a rebuild
  without the wrapper; `config-data` untouched; no wrapper text in the build
  output.

## Log

- 2026-10-01: Plan approved. Worktree and branch created from master
  `47c67f5`. Contract file written; the build-speed session
  (`speedup-build-d4`) was not reachable by direct message, so coordination
  runs through the contract file. Questions a-d in the contract are open.
- 2026-10-01: M0a implemented and smoke-tested; spec §18 and D15 written;
  sent to the twin review.

## Review verdicts

(M0a: pending)

## Next step

Take M0a through the review gate. Then M0b: per-family argument parser (GCC,
Clang), platform fingerprint, compiler identity, environment digest, the key,
richer `events.jsonl` lines, and `cactup cache report`.
