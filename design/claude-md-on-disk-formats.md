# Proposed CLAUDE.md text: on-disk formats

Drop-in replacement for the section "## On-disk formats: no backward
compatibility (being softened)" of `/home/max/src/Cactup/CLAUDE.md`
(untracked, so not edited here). Written against the code of `formats/p3`.

## Line 3

Replace

> Source of truth: `design/cactup-simfactory-design-new.md` §2.4 records these
> same contracts in the spec; this file is the working summary.

with

> Source of truth: `design/cactup-simfactory-design-new.md` §2.4 (interrupts,
> progress, §-hygiene) and §2.5 (on-disk formats) record these same contracts
> in the spec; this file is the working summary.

(Check the existing wording of line 3 and keep it; only the pointer changes.)

## The section

Everything between the rules below is the replacement text.

---

## On-disk formats: versioned, read old, refuse newer, keep unknown keys (spec §2.5)

Every file cactup writes and reads back goes through `format::read` /
`format::write` (`src/format.rs`), described by its own `Format` static in the
module that owns the record (`database::DATABASE`, `sim::restart::RESTART`,
`build::CONFIG`, …; `grep -rn "Format<" src` lists them all). Local records
carry `schema`, the shared crate's interchange formats `format`; a missing key
is 1. Structs never hold the version: it belongs to the file, and the helper
strips it before the typed parse and writes it back first. Never call
`toml::from_str`, `serde_json::from_str` or a bare `fs::write` on a record.

- **Read: version first.** A newer file is refused with one upgrade message
  naming the file ("written by a newer cactup … upgrade cactup"), before any
  typed parse, and is never overwritten. An older one runs the format's
  `legacy` normalization and its `vN → vN+1` upgrades over the raw document,
  then parses. **A read never rewrites the file** (it races concurrent
  writers); an old file is upgraded on disk by its next save. Upgrades are kept
  forever. Only a raw document that does not parse is retried, never a newer
  version.
- **Write: atomic, sticky, keeps what it does not know.** Temp file in the
  same directory, fsync, rename. The write re-reads the target: a newer file
  is refused; the version written is the oldest one, no older than the file on
  disk, that the down-chain reaches while the content fits; keys on disk that
  this binary does not model are put back (maps only, only where absent), so
  an older binary's rewrite keeps a newer one's additions.
- **`write` vs `replace`.** `write` edits the record on disk and puts unknown
  keys back. `replace` writes a record made anew (a build finishing, a test
  re-run in the same directory, a frozen copy): same refusal and sticky
  version, nothing put back. A sub-record a writer always rebuilds whole goes
  in `Format::replace_whole` (or `write_replacing` for one write), or a key a
  newer binary added to the old one would outlive it. A secret goes through
  `write_private` (0600 from creation, whatever the umask or the old mode).
  Other new files get 0666 less the umask; existing files keep their mode (a
  Cactus tree is group-shared).
- **Additive needs no bump**: a new optional key whose absence means the old
  behavior and that an older binary may safely ignore. Records are not
  `deny_unknown_fields`. `skip_serializing_if` may only skip the field's
  deserialize default, or a written value would come back as "unknown".
- **Anything else bumps**: rename, retype, removal, new meaning, stricter
  validation, a new required key, a key an older binary would get wrong by
  ignoring it. A `legacy` hook is a closed list of normalizations that shipped
  before versioning (`detected-machine`, `variant`); do not add to it, bump.
- **Frozen readers.** A queued job runs the binary that submitted it for months
  (`@CACTUP@`) against files a newer login binary rewrites. Formats such a job
  reads (`simulation.toml`, `restart.toml`, `build.toml`, `test.toml`,
  `cactup-config.toml`) are `frozen_read`: a write uses the oldest version that
  holds the content, a breaking bump ships its `vN+1 → vN` down-step in
  `Format::downgrades` (a test enforces `len == current - down_floor`), and
  nothing an older binary must understand is written into an **existing**
  record of one (a record created by the newer binary may carry the new
  version). A down-step answers `CannotExpress` when the document uses
  something the older version cannot hold, so only files using the feature get
  the new version.
- **Fixtures and the shape pin.** `fixtures/formats/<format>/` keeps a file
  per shipped shape plus `current.*`; a committed fixture is never edited or
  deleted (except `current.*`), and every one must load. Each format has an
  `exhaustive_sample()` (struct literal without `..`) pinned against
  `current.*`. When the pin fails: additive → copy the old `current` to a new
  numbered fixture and re-pin; otherwise bump.
- **Interchange formats** have deployed readers we cannot update. `.sim`
  writes the oldest generation that holds its content
  (`SimFile::required_format`) and reads `1..=SIM_FORMAT`; manifests are read
  `1..=N` forever (tags are immutable); the stack release's `format` is
  checked against `STACK_FORMAT` before the typed parse; the API is gated by
  `api-generation`. **`latest.json` is never versioned and never broken**: old
  binaries update themselves from it. Add keys only; a change that cannot be
  additive is published at a new path beside it (the path is the version).
- **Deliberately unversioned:** caches (the build cache's `store::FORMAT` /
  `KEY_LABEL` namespaces, memos, `events.jsonl`, the inputs store) — a change
  is a fresh namespace or a miss; lock stamps (append lines only; readers
  ignore extras); `log.txt` (simfactory's); CRL thornlists (external);
  `cc/config.toml` (it names the exact binary that reads it). The MDB has its
  own generation (below).

**Adding a format.** Write the record's struct without a version field; add a
`pub static NAME: Format<Toml>` (or `Json`) next to it with `current: 1`, empty
chains, `down_floor: 1`, `frozen_read` true iff a queued job's binary reads it,
and `replace_whole` for every sub-record a writer rebuilds. Read and write only
through `format::*`. Add `fixtures/formats/<name>/` (a `v1.*` and `current.*`;
a `-noschema` shape if files of it already exist without the key), an
`exhaustive_sample()`, and the generic tests of `format::testing` on the
descriptor.

**Bumping a format.** Increment `current`; append the `vN → vN+1` function to
`upgrades` (it works on the raw document, never on the struct); add the
`vN+1.*` fixture and re-pin `current.*`; update the matrix in spec §2.5. If
the format is `frozen_read`, also append the `vN+1 → vN` function to
`downgrades` (returning `CannotExpress` where the content needs `vN+1`), or
the descriptor test fails. A bump of a login-only format makes older login
binaries refuse the file with "upgrade cactup": prefer the additive route.
When a bump would force people to re-create state (a built config above all),
ask before assuming it is acceptable.

**Config records (spec §7.4).** The record's schema 1 → 2 upgrade
(`variant` → `flavor`, flat `gpu`/`compatible-queues` → `[runs-on]`) is the
first entry of `build::CONFIG`'s `upgrades`; it covers `cactup-config.toml`
and the copy embedded in each build attempt's `build.toml` (`parse_embedded` /
`upgrade_embedded`). Do not remove it: a build queued by an older binary
writes schema 1 when it finishes. `CONFIG.down_floor` is 2: that bump predates
down-steps, so its narrow window (spec §7.4) is accepted; every later bump
ships its down-step.

---
