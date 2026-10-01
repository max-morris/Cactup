# Contract text for `CLAUDE.md` (not applied yet)

`CLAUDE.md` is not tracked by git: it lives only in the main checkout
(`/home/max/src/Cactup/CLAUDE.md`, listed in `.git/info/exclude`) and every
session working in the repository reads it. Editing it from this branch
would put a contract in front of sessions whose tree does not have the code
it describes. So the text below waits here until the build cache lands on
master, and is then added to `CLAUDE.md` before the "On-disk formats"
section, with Max's go-ahead. The spec carries the same seven rules now (§18.1).

---

## Build cache: it may only ever cost a miss (spec D15, §18)

`src/objcache/` puts cactup in front of Cactus's object compiles as a
compiler wrapper (`cactup __cc …`, dispatched at the very top of `main`).
It runs thousands of times per build with `make` waiting on it. The rules
are spec §18.1; hold them when touching it:

- **Never a stale object.** A hit must be byte for byte what the compile
  would produce here and now. Whatever a key or an adapter does not fully
  understand is not cached: a false miss is fine, a false hit is not.
- **Fail open.** The cache never fails a build and never changes what gets
  compiled. Every error path before the compiler has run — unreadable
  configuration, unknown flag, I/O error, a panic (the release profile
  aborts on one, so the wrapper installs a hook) — ends in `pass_through`,
  which `exec`s the real compiler exactly as `make` asked. After it has
  run, its exit status is the wrapper's, whatever else goes wrong.
- **Nothing outside Cactus's object compiles.** Injection is the fragment
  the probe writes: it redefines Cactus's compile recipes, only inside
  Cactus's object sub-makes, and is read through `MAKEFILES`. Never set a
  compiler variable (not per target either: before GNU make 4.4 that leaks
  into the environment of prerequisite recipes), never a `make CC=…`
  override, never an optionlist rewrite. ExternalLibraries builds,
  configure runs, dependency generation and `config-data` must not see
  cactup.
- **Quiet.** The wrapper's stdout and stderr are the compiler's. Nothing of
  cactup's goes there on a compile that runs (`CACTUP_CC_DEBUG` is the
  opt-in exception). A whole build going uncached gets one line.
- **Signals and exit statuses pass through.** Forward stop signals to the
  compiler, leave ignored ones ignored (`nohup`), and end the way the
  compiler ended. `rustix` is there for `kill(2)`/`waitpid(2)` only.
- **Hermetic (D11).** The wrapper and the probe read the attempt's frozen
  `cc/config.toml` and the configuration directory named in it — never the
  global DB, the registry, the MDB, or knobs. Cache knobs are resolved in
  `prepare` and frozen.
- **No eviction on its own**, and no flock: the store is lock-free
  (immutable entries, link-based publication), like everything else under
  `$CACTUP_HOME`.

The interface this relies on in Cactus's make system, and the agreement
with the separate build-speed work, is `design/build-cache/` and the
contract file it points to.
