//! The build engine behind `cactup config build` (spec §7):
//! optionlist selection + render + flag injection (§7.8), thornlist toggles
//! (§7.5, D8), env-setup'd `make` driving (§7.2, §6.1), build universes
//! (§4.8), the rebuild-decision snapshot diff (§7.8), the per-config build
//! lock (§2.3 item 4), and `cactup-config.toml` metadata (§7.4).

use crate::args::{BuildOpts, MakeJobs};
use crate::database::SCHEMA;
use crate::fetch::SourceHeads;
use crate::installation::Installation;
use crate::lock::LinkLock;
use crate::mdb::meta::Phase;
use crate::mdb::{Machine, Optionlist};
use crate::template::{VarSet, VarValue};
use crate::thornlist::Thornlist;
use crate::Res;
use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Make command used when a machine's `meta.toml` omits `[build].make`. It
/// templates `@MAKEJOBS@` so `[build].make-jobs` (§7.6) is honored as the
/// default `-j` without every machine having to hand-write the token.
const DEFAULT_MAKE: &str = "make -j@MAKEJOBS@";

/// The `-j max` expansion: count the CPUs available to the build shell at
/// runtime. `nproc` honors the process's cpuset/affinity, so inside an
/// srun/singularity wrapper it reports that allocation, not the login node.
/// The `|| echo 1` fallback matters for safety: if `nproc` were missing, a bare
/// `make -j` (empty count) means *unbounded* parallelism, so we degrade to 1.
const MAX_MAKEJOBS: &str = "$(nproc 2>/dev/null || echo 1)";

/// Resolve the `@MAKEJOBS@` build variable (§7.6): `--make-jobs` > machine
/// `make-jobs` > 1. `-j max` becomes `MAX_MAKEJOBS`, a shell expression the
/// build shell itself evaluates — inside the universe wrapper when there is one
/// — so it counts the CPUs actually available in that context rather than on
/// the login node cactup is invoked on. Any explicit count is a plain integer.
fn make_jobs_var(cli: Option<MakeJobs>, machine_default: Option<u32>) -> VarValue {
    match cli {
        Some(MakeJobs::Max) => VarValue::Str(MAX_MAKEJOBS.to_owned()),
        Some(MakeJobs::Count(n)) => VarValue::Int(n as i64),
        None => VarValue::Int(machine_default.unwrap_or(1) as i64),
    }
}

fn default_schema() -> u32 {
    SCHEMA
}
fn default_true() -> bool {
    true
}

/// `configs/<name>/cactup-config.toml` (§7.4). `built` is a cactup extension
/// used by `config list`/`show` and the §7.1 most-recently-built repoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConfigMeta {
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub name: String,
    /// Optionlist variant used.
    pub variant: String,
    /// Snapshotted from the optionlist `[cactup]` header at build time (D12).
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub compatible_queues: Vec<String>,
    pub thornlist: String,
    /// A build is not portable across machines.
    pub machine: String,
    /// Resolved build universe; omitted for the host context (§4.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universe: Option<String>,
    #[serde(default = "default_true")]
    pub coerce_run_universe: bool,
    pub config_id: String,
    pub build_id: String,
    #[serde(default)]
    pub built: Option<DateTime<Utc>>,
    #[serde(default)]
    pub flags: BuildFlags,
    /// Per-repo HEADs (from `fetch-state.toml`) of the repos this config's
    /// thorns came from, as of this build — the §7.4 source-tracking input
    /// that makes a refetch invalidate the builds it actually reaches.
    /// Absent for configs built before source tracking landed and for
    /// installations with no fetch record; absent reads as "no information",
    /// never as "unchanged". Serialized last: it is a TOML table, and a table
    /// may not precede scalar keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<BTreeMap<String, String>>,
    /// Thorn name -> providing directory (from the processed thornlist,
    /// `Thornlist::thorn_providers`), as of this build — the §7.4 input that
    /// lets a rebuild notice a thorn name changing provider. The motivating
    /// incident: Cactus keys `configs/<cfg>/build/<Thorn>/` and
    /// `libthorn_<Thorn>.a` by thorn *name* only, so swapping which
    /// arrangement provides a name (e.g. disabling `EinsteinAnalysis/Foo` and
    /// enabling `SpacetimeX/Foo`) silently reuses build state compiled from
    /// the other source tree — a stale `.d` file can name a bindings header
    /// the reconfigure correctly deleted (a hard make error), and `ar`
    /// updates an existing `libthorn_*.a` in place, so stale members from the
    /// old provider can survive into the link without so much as a warning.
    /// Absent reads as "no information", never "unchanged" — same convention
    /// as `sources`. Serialized last, alongside `sources`: it is a TOML
    /// table, and a table may not precede scalar keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thorn_providers: Option<BTreeMap<String, String>>,
    /// Thorn name -> `thorn_shapes` fingerprint, as of this build — the §7.4
    /// input that catches every OTHER way a thorn's content changes while its
    /// thornlist path stays identical (see `thorn_shapes`'s doc comment for
    /// the two motivating incidents: a `.ccl` REQUIRES edit and a removed
    /// source file, both of which `thorn_providers`/`provider_delta` are
    /// blind to since neither moves which directory provides the name).
    /// Absent reads as "no information", never as "unchanged" — same
    /// convention as `sources` and `thorn_providers`. Serialized last,
    /// alongside them: it is a TOML table, and a table may not precede
    /// scalar keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thorn_shapes: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BuildFlags {
    #[serde(default)]
    pub debug: bool,
    #[serde(default = "default_true")]
    pub optimize: bool,
    #[serde(rename = "unsafe", default)]
    pub unsafe_build: bool,
    #[serde(default)]
    pub profile: bool,
}

/// Optimized is the one flag that defaults ON (§7.6) — must match the serde
/// default above so a missing `[flags]` table and `BuildFlags::default()`
/// agree.
impl Default for BuildFlags {
    fn default() -> Self {
        BuildFlags { debug: false, optimize: true, unsafe_build: false, profile: false }
    }
}

impl ConfigMeta {
    pub fn path_for(cactus_root: &Path, name: &str) -> PathBuf {
        cactus_root.join("configs").join(name).join("cactup-config.toml")
    }

    pub fn load(cactus_root: &Path, name: &str) -> Res<Option<ConfigMeta>> {
        let path = Self::path_for(cactus_root, name);
        let text = match fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            other => other.with_context(|| format!("Failed to read {}", path.display()))?,
        };
        let meta: ConfigMeta =
            toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;
        if meta.schema > SCHEMA {
            bail!(
                "{} has schema {} (> {SCHEMA}); please upgrade cactup",
                path.display(),
                meta.schema
            );
        }
        Ok(Some(meta))
    }

    fn store(&self, cactus_root: &Path) -> Res<()> {
        let path = Self::path_for(cactus_root, &self.name);
        fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("Failed to write {}", path.display()))
    }
}

/// The two thornlist artifacts kept in a config directory (§7.5). The
/// *processed* copy (toggles applied) is what Cactus is handed as `THORNLIST=`,
/// and it is what the rebuild decision diffs. The *snapshot* is the source file
/// byte for byte, so a config stays rebuildable after the file it was built
/// from moves or is deleted — the same role `cactup-optionlist.toml` plays for
/// the optionlist.
/// Public because `sim create` copies both into a simulation's `.cactup/cfg/`
/// for provenance (§8.2) — a rename here must not silently break that.
pub const THORNLIST_PROCESSED: &str = "cactup-thornlist.th";
pub const THORNLIST_SNAPSHOT: &str = "cactup-thornlist.src.th";

fn config_file(cactus_root: &Path, name: &str, file: &str) -> PathBuf {
    cactus_root.join("configs").join(name).join(file)
}

/// The source thornlist a build will use, and where it came from (§7.5).
pub struct ResolvedThornlist {
    /// Recorded in `cactup-config.toml`'s `thornlist` key: the original source
    /// path, preserved even when the snapshot had to stand in for it.
    pub recorded: String,
    /// Verbatim source text, before thorn toggles.
    pub text: String,
    /// Set when `recorded` was unreadable and the config's snapshot was used.
    pub from_snapshot: bool,
}

/// Resolve the source thornlist for a build (§7.5):
///
///   1. `--thornlist PATH` — explicit; a hard error if unreadable.
///   2. the path this config was last built from (stored metadata).
///   3. that config's verbatim snapshot, when the stored path has since moved
///      or been deleted.
///   4. `<Cactus root>/thornlists/installation-default.th` — the fresh-config
///      default (the pre-rename `einsteintoolkit.th` on an installation the
///      §3.2 name migration has not reached).
///
/// Steps 2-3 are why a rebuild no longer silently reverts a config built from a
/// custom thornlist to the stock Einstein Toolkit list: the flag need not be
/// repeated on every rebuild, and the snapshot means the original file going
/// away cannot quietly change what gets built. Step 2 preferring the live file
/// over the snapshot is deliberate — editing the thornlist in place is the
/// normal way to add a thorn, and that edit must be picked up.
pub fn resolve_thornlist(
    cactus_root: &Path,
    name: &str,
    stored: Option<&ConfigMeta>,
    cli: Option<&Path>,
) -> Res<ResolvedThornlist> {
    if let Some(path) = cli {
        let text = fs::read_to_string(path)
            .with_context(|| format!("Failed to read thornlist {}", path.display()))?;
        // Canonicalize what we record: a relative path would resolve against
        // whatever directory a later rebuild happened to run from.
        let recorded = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        return Ok(ResolvedThornlist {
            recorded: recorded.display().to_string(),
            text,
            from_snapshot: false,
        });
    }

    if let Some(stored_path) = stored.map(|m| m.thornlist.as_str()) {
        if let Ok(text) = fs::read_to_string(stored_path) {
            return Ok(ResolvedThornlist {
                recorded: stored_path.to_owned(),
                text,
                from_snapshot: false,
            });
        }
        // A path recorded under the pre-rename thornlist name whose file is
        // gone is almost certainly the §3.2 rename, not a deleted file: look
        // for the renamed one beside it before falling back to the snapshot,
        // so an installation the migration could not retarget (a config copied
        // in from elsewhere, say) still rebuilds from the live list.
        if let Some(renamed) = renamed_legacy_thornlist(Path::new(stored_path)) {
            if let Ok(text) = fs::read_to_string(&renamed) {
                return Ok(ResolvedThornlist {
                    recorded: renamed.display().to_string(),
                    text,
                    from_snapshot: false,
                });
            }
        }
        let snapshot = config_file(cactus_root, name, THORNLIST_SNAPSHOT);
        let text = fs::read_to_string(&snapshot).with_context(|| {
            format!(
                "config \"{name}\" was built from thornlist {stored_path}, which is no longer \
                 readable, and there is no snapshot at {} to fall back on — pass --thornlist to \
                 say which thornlist to build from",
                snapshot.display()
            )
        })?;
        return Ok(ResolvedThornlist {
            recorded: stored_path.to_owned(),
            text,
            from_snapshot: true,
        });
    }

    let default = default_thornlist(cactus_root);
    let text = fs::read_to_string(&default)
        .with_context(|| format!("Failed to read thornlist {}", default.display()))?;
    Ok(ResolvedThornlist {
        recorded: default.display().to_string(),
        text,
        from_snapshot: false,
    })
}

/// The live thornlist a fresh config builds from (rule 4 above), by Cactus root
/// rather than by [`Installation`](crate::installation::Installation) — build
/// resolution is given only the root. Falls back to the pre-rename name for an
/// un-migrated installation, exactly as `Installation::live_thornlist_to_read`
/// does.
pub fn default_thornlist(cactus_root: &Path) -> PathBuf {
    let dir = cactus_root.join("thornlists");
    let current = dir.join(crate::installation::LIVE_THORNLIST);
    let legacy = dir.join(crate::installation::LEGACY_THORNLIST);
    if current.exists() || !legacy.exists() {
        current
    } else {
        legacy
    }
}

/// `path` with the pre-rename thornlist name swapped for the current live one,
/// or `None` if `path` does not end in the pre-rename name.
fn renamed_legacy_thornlist(path: &Path) -> Option<PathBuf> {
    (path.file_name()? == crate::installation::LEGACY_THORNLIST)
        .then(|| path.with_file_name(crate::installation::LIVE_THORNLIST))
}

/// Point configs that recorded the live thornlist under its pre-rename name at
/// the renamed file. Part of `Installation::migrate_thornlist_names`; a config
/// whose recorded path is anything else (an explicit `--thornlist` file) is
/// left alone. Returns how many configs were retargeted.
///
/// A config left pointing at the vanished old name would still build — via its
/// snapshot (`resolve_thornlist` rule 3) — but it would stop picking up edits
/// to the live list, which is the whole point of rule 2.
pub fn retarget_recorded_thornlist(
    cactus_root: &Path,
    legacy_live: &Path,
    live: &Path,
) -> Res<usize> {
    let legacy_display = legacy_live.display().to_string();
    let live_display = live.display().to_string();
    let mut retargeted = 0;
    for (name, meta) in crate::commands::config::list_configs(cactus_root)? {
        let Some(mut meta) = meta else { continue };
        // The recorded path is either what rule 4 wrote (a plain `display()`
        // of the live path) or, when the same file was named via
        // `--thornlist`, its canonicalized form. The old file is gone by now,
        // so match on the name and directory instead of canonicalizing it.
        let recorded = Path::new(&meta.thornlist);
        let same_dir = || match (recorded.parent(), legacy_live.parent()) {
            (Some(a), Some(b)) => {
                a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
            }
            _ => false,
        };
        let is_legacy_live = meta.thornlist == legacy_display
            || (recorded.file_name() == Some(crate::installation::LEGACY_THORNLIST.as_ref())
                && same_dir());
        if !is_legacy_live {
            continue;
        }
        meta.thornlist = live_display.clone();
        meta.store(cactus_root)
            .with_context(|| format!("Failed to retarget config \"{name}\"'s thornlist"))?;
        retargeted += 1;
    }
    Ok(retargeted)
}

/// Apply the §7.5 (D8) machine thorn toggles to a thornlist's contents:
/// `disabled-thorns` entries get a `#DISABLED ` prefix, `enabled-thorns`
/// entries get it removed. Entries match a thorn line's `arrangement/Thorn`
/// (or bare thorn name after `/`).
pub fn apply_thorn_toggles(thornlist: &str, enabled: &[String], disabled: &[String]) -> String {
    let matches = |spec: &str, thorn: &str| -> bool {
        thorn == spec || thorn.rsplit('/').next() == Some(spec)
    };
    thornlist
        .lines()
        .map(|line| {
            let bare = line.strip_prefix("#DISABLED ").unwrap_or(line);
            let thorn = bare.trim();
            if thorn.is_empty() || thorn.starts_with('#') || thorn.starts_with('!') {
                return line.to_owned();
            }
            if disabled.iter().any(|d| matches(d, thorn)) {
                format!("#DISABLED {bare}")
            } else if line.starts_with("#DISABLED ") && enabled.iter().any(|e| matches(e, thorn)) {
                bare.to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// §7.8 rule 5: inject the effective build flags into the rendered native
/// optionlist — replace an existing `KEY = …` line or append. Note the
/// deliberate American→British mapping: `optimize` drives Cactus's
/// `OPTIMISE` key (external identifier, emitted verbatim).
pub fn inject_build_flags(rendered: &str, flags: BuildFlags) -> String {
    let mut out: Vec<String> = rendered.lines().map(str::to_owned).collect();
    for (key, on) in [
        ("DEBUG", flags.debug),
        ("OPTIMISE", flags.optimize),
        ("UNSAFE", flags.unsafe_build),
        ("PROFILE", flags.profile),
    ] {
        let value = format!("{key} = {}", if on { "yes" } else { "no" });
        match out.iter_mut().find(|l| {
            l.split_once('=')
                .is_some_and(|(k, _)| k.trim() == key)
        }) {
            Some(line) => *line = value,
            None => out.push(value),
        }
    }
    out.join("\n") + "\n"
}

/// The effective flag set: CLI > stored config metadata > default (§7.6).
/// CLI booleans can only turn a flag ON; turning one off means editing the
/// optionlist or rebuilding fresh.
pub fn effective_flags(opts: &BuildOpts, stored: Option<BuildFlags>) -> BuildFlags {
    let base = stored.unwrap_or_default();
    BuildFlags {
        debug: opts.debug || base.debug,
        optimize: opts.optimize || base.optimize,
        unsafe_build: opts.unsafe_build || base.unsafe_build,
        profile: opts.profile || base.profile,
    }
}

/// Whether a build must run, and how much of it (§7.8).
#[derive(Debug, PartialEq)]
pub enum RebuildDecision {
    /// No config on disk yet.
    Fresh,
    /// Nothing cactup tracks changed: a complete config can short-circuit.
    UpToDate,
    /// The thorn set changed: reconfigure + `make`, but no `realclean`. Unlike
    /// an optionlist edit this does not invalidate already-compiled objects —
    /// it changes *which* thorns are in the build, not how the code compiles —
    /// and Cactus regenerates the bindings itself off the
    /// `configs/<name>/ThornList` that the reconfigure step copies into place.
    /// Adding a thorn is routine, so paying a from-scratch rebuild for it would
    /// be a poor trade; `-f` is still there when one is wanted.
    ///
    /// One exception, discovered the hard way: a thorn *name* that persists
    /// across the change while the directory providing it changes (a
    /// `#DISABLED`/enable swap between two arrangements, e.g.
    /// `EinsteinAnalysis/WeylScal4` for `SpacetimeX/WeylScal4`) really does
    /// change how that name's code compiles, because Cactus keys
    /// `build/<Thorn>/` and `libthorn_<Thorn>.a` by name only. The build path
    /// keeps this variant safe by deleting the affected per-thorn state
    /// before invoking make — see `provider_delta` and its call site in
    /// `build()`.
    ///
    /// A second, sibling per-thorn invalidation trigger: a thorn's *content*
    /// changing shape (a `.ccl` `REQUIRES` edit, a removed source file) while
    /// its thornlist path stays byte-identical, so nothing else here would
    /// otherwise notice. See `thorn_shapes`/`shape_delta` and their call site
    /// in `build()`, which deletes the same per-thorn state for the same
    /// reason as the provider-swap case above.
    Incremental(&'static str),
    /// Optionlist or universe changed: realclean + reconfigure + build. Both
    /// change *how* the sources compile, so every existing object is suspect.
    Full(&'static str),
}

/// How the source trees under a config differ from what it was built with
/// (§7.4). A refetch is only one of the ways this happens — editing a thorn in
/// place, or `git checkout` inside a repo, are ordinary workflows too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceDelta {
    /// Nothing inspectable, or nothing stored on the config — behave exactly
    /// as cactup did before source tracking.
    Unknown,
    Unchanged,
    /// Tracked files differ from what was built: someone is editing sources in
    /// place. `make`'s own dependency tracking decides what that costs, so
    /// this never escalates past a reconfigure — including for the flesh,
    /// where a from-scratch rebuild would punish anyone iterating on it.
    Edited,
    /// One or more thorn repos are on a different commit: recompile what that
    /// affects.
    Thorns,
    /// The Cactus flesh is on a different commit. That is the make system and
    /// everything `config-data/cctk_Config.h` is generated from, so every
    /// existing object is suspect — the one source change earning a realclean.
    Flesh,
}

/// What changed under a config, and which repos are responsible.
#[derive(Debug, Default)]
pub struct SourceChange {
    /// Repos now on a different commit than the build used.
    pub moved: Vec<String>,
    /// Repos whose worktree differs from what the build used.
    pub edited: Vec<String>,
}

impl SourceChange {
    fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.edited.is_empty()
    }
}

/// Compare the source state recorded at the last build against the tree as it
/// stands now, returning the delta and the repos responsible.
///
/// A repo *missing* from the stored record is not a change on its own: the
/// first build after source tracking landed, and any config whose thornlist
/// just gained a thorn, would otherwise report every repo as new. A thornlist
/// that gained or dropped a thorn is already caught by the processed-thornlist
/// diff, so nothing is lost by only comparing repos both sides know about.
pub fn source_delta(
    stored: Option<&BTreeMap<String, String>>,
    fresh: Option<&SourceHeads>,
) -> (SourceDelta, SourceChange) {
    let (Some(stored), Some(fresh)) = (stored, fresh) else {
        return (SourceDelta::Unknown, SourceChange::default());
    };
    let mut change = SourceChange::default();
    for (repo, state) in &fresh.heads {
        let Some(was) = stored.get(repo.as_str()) else { continue };
        if was == state {
            continue;
        }
        // Split by *why* it differs: the commit moved (a refetch or a manual
        // checkout) or only the worktree did (a hand edit).
        if crate::fetch::committed(was) == crate::fetch::committed(state) {
            change.edited.push(repo.clone());
        } else {
            change.moved.push(repo.clone());
        }
    }
    if change.is_empty() {
        return (SourceDelta::Unchanged, change);
    }
    let delta = if fresh.flesh.as_deref().is_some_and(|f| change.moved.iter().any(|m| m == f)) {
        SourceDelta::Flesh
    } else if !change.moved.is_empty() {
        SourceDelta::Thorns
    } else {
        SourceDelta::Edited
    };
    (delta, change)
}

/// The §7.4 provenance counterpart to `source_delta`: which thorn *names*
/// need their per-thorn build state (`build/<Thorn>/`, `libthorn_<Thorn>.a`)
/// invalidated because the directory providing that name changed. Returns
/// names present in `stored` whose `fresh` provider differs, plus names
/// present in `stored` but absent from `fresh` (sorted, free via `BTreeMap`
/// iteration order).
///
/// A dropped thorn is included deliberately: its `build/<Thorn>/` must be
/// removed too, or re-adding the name later from a *different* provider finds
/// no baseline to diff against and the stale state silently survives. Names
/// only in `fresh` (newly added thorns) are not a change — there is nothing
/// built yet to invalidate — mirroring `source_delta`'s bootstrap tolerance
/// for repos the stored record never knew about.
///
/// Either side `None` means no information (a build predating provenance
/// tracking, or a thornlist cactup could not parse), never "unchanged" — so
/// this returns empty rather than guessing.
pub fn provider_delta(
    stored: Option<&BTreeMap<String, String>>,
    fresh: Option<&BTreeMap<String, String>>,
) -> Vec<String> {
    let (Some(stored), Some(fresh)) = (stored, fresh) else {
        return Vec::new();
    };
    stored
        .iter()
        .filter(|(name, provider)| fresh.get(name.as_str()) != Some(provider))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Names of files/dirs `thorn_shapes` never walks into once past a thorn's
/// top level — the ones we *do* walk (a bare file, or `src/` recursively) are
/// simpler to say positively, so this only exists to name them in one place
/// for the doc comment above.
const SHAPE_QUALIFYING_STEMS: [&str; 3] =
    ["make.code.defn", "make.configuration.defn", "make.code.deps"];

/// A file's basename decides `.ccl`-family generation/compilation inputs
/// (item 5 of `thorn_shapes`): any `*.ccl`, or one of the three fixed
/// `make.*` filenames Cactus's build system reads per-thorn.
fn is_shape_qualifying(basename: &str) -> bool {
    basename.ends_with(".ccl") || SHAPE_QUALIFYING_STEMS.contains(&basename)
}

/// Depth cap for the recursive `src/` walk below: a thorn dir is normally a
/// symlink, and following symlinks (needed to reach the real tree) means a
/// hostile or accidental symlink cycle under `src/` could recurse forever.
/// 32 is far deeper than any real thorn's source tree.
const SHAPE_WALK_MAX_DEPTH: u32 = 32;

/// Recursively collect file paths under `dir` (a thorn's `src/`, or a
/// directory beneath it), relative to the thorn dir, into `out`. Symlinks are
/// followed (`fs::metadata`, not `symlink_metadata`) since the thorn dir
/// itself is normally one; `depth` guards against a symlink loop hanging the
/// build by simply declining to recurse past `SHAPE_WALK_MAX_DEPTH` rather
/// than erroring — silent truncation is fine here, since it can only make a
/// fingerprint miss part of an already-pathological tree, not corrupt one.
fn walk_shape_dir(dir: &Path, prefix: &str, depth: u32, out: &mut Vec<String>) -> std::io::Result<()> {
    if depth > SHAPE_WALK_MAX_DEPTH {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let rel = format!("{prefix}/{name}");
        let meta = fs::metadata(entry.path())?;
        if meta.is_dir() {
            walk_shape_dir(&entry.path(), &rel, depth + 1, out)?;
        } else if meta.is_file() {
            out.push(rel);
        }
    }
    Ok(())
}

/// Item 4 of `thorn_shapes`: the compilation-relevant file paths under a
/// thorn dir, relative to it — direct children, plus everything recursively
/// under `src/`. `doc/`, `test/`, `par/`, `.git`, and any other subdirectory
/// are deliberately not descended into: none of them feed the compile or the
/// bindings generation this fingerprint exists to track.
fn shape_files(thorn_dir: &Path) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(thorn_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().into_owned();
        let meta = fs::metadata(entry.path())?;
        if meta.is_dir() {
            if name_str == "src" {
                walk_shape_dir(&entry.path(), "src", 0, &mut files)?;
            }
        } else if meta.is_file() {
            files.push(name_str);
        }
    }
    files.sort();
    Ok(files)
}

/// If `real` (a canonicalized thorn dir) lands under `repos_root`
/// (canonicalized `<cactus_root>/repos`), the repo directory that provides
/// it — i.e. `repos_root` plus just the first path component past it, so a
/// thorn checked out via `!REPO_PATH` into a subdirectory of the repo still
/// resolves to the repo itself, not that subdirectory.
fn shape_repo_dir(real: &Path, repos_root: &Path) -> Option<PathBuf> {
    let rel = real.strip_prefix(repos_root).ok()?;
    let repo_name = rel.components().next()?;
    Some(repos_root.join(repo_name.as_os_str()))
}

/// `origin`'s normalized fetch URL for the repo at `repo_dir`, cached across
/// calls: ~30 thorns can share one repo, and reopening it per thorn would be
/// wasteful on a real ~400-thorn tree. `None` (unreadable repo, no `origin`,
/// no fetch URL) is cached too, so a repo that fails once isn't retried for
/// every thorn it provides.
fn shape_repo_url(repo_dir: &Path, cache: &mut HashMap<PathBuf, Option<String>>) -> Option<String> {
    if let Some(cached) = cache.get(repo_dir) {
        return cached.clone();
    }
    let url = (|| {
        let repo = gix::open(repo_dir).ok()?;
        let remote = repo.find_remote("origin").ok()?;
        let raw = remote.url(gix::remote::Direction::Fetch)?.to_bstring().to_string();
        Some(crate::fetch::git::normalize_url(&raw))
    })();
    cache.insert(repo_dir.to_owned(), url.clone());
    url
}

/// Feed one length-prefixed frame (`u64` little-endian byte length, then the
/// bytes themselves) into `hasher`. This is what makes the byte stream
/// `thorn_shapes` hashes unambiguous: a length-prefixed frame is a prefix
/// code, so the concatenation of frames for one thorn can only be produced by
/// that exact sequence of fields — no frame boundary can be mistaken for
/// content, and (since item 3 below is omitted outright rather than replaced
/// by a placeholder when it doesn't apply) a thorn with N frames can never
/// coincide with one with a different frame count.
fn feed(hasher: &mut gix::hash::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Per-thorn "shape" fingerprints: thorn name -> hash of everything that
/// decides WHAT is compiled into `build/<Thorn>/` and what bindings are
/// generated for it. The sibling of `thorn_providers`/`provider_delta`: those
/// catch a thorn *name* silently changing which directory provides it; this
/// catches every other way a thorn's content changes while its thornlist path
/// stays byte-identical, so nothing about the thornlist diff or `provider_delta`
/// would notice. Two incidents motivate it:
///
///   - A thorn's `configuration.ccl` `REQUIRES` goes non-empty -> empty across
///     a refetch. Cactus's CST deletes the now-unneeded
///     `bindings/Configuration/Thorns/cctki_<Thorn>.h`, but the stale
///     `build/<Thorn>/*.d` still lists it as a prerequisite: `make: *** No
///     rule to make target '.../cctki_<Thorn>.h'` — a hard build failure.
///   - A source file is removed from a thorn. Its `.o` survives in
///     `build/<Thorn>/`, and Cactus updates `libthorn_<Thorn>.a` in place
///     with `ar`, so the orphan object links in silently.
///
/// For each `list.thorn_providers()` entry (thorn name -> provider path, e.g.
/// `arrangements/SpacetimeX/WeylScal4`, relative to `cactus_root`), the
/// following are hashed in order, each as a `feed` frame (see its doc
/// comment for why that makes the stream unambiguous):
///
///   1. the provider path itself;
///   2. the raw `fs::read_link` target of `<cactus_root>/<provider path>` if
///      it is a symlink (normally the case), or the fixed marker `<dir>`
///      when it is a real directory;
///   3. the backing repo's normalized `origin` fetch URL, when the provider
///      path resolves under `<cactus_root>/repos/<repo>` — normalized via
///      `git::normalize_url` so a mere URL-spelling change (not a fork/repoint)
///      is not an identity change. Omitted entirely (not a placeholder) when
///      the thorn does not resolve into `repos/`, e.g. a hand-placed
///      arrangement or a test fixture;
///   4. the sorted list of file paths under the thorn (`shape_files`,
///      relative to the thorn dir), one frame per path;
///   5. for each of those files whose basename is `*.ccl` or one of the fixed
///      `make.*.defn`/`make.code.deps` names (`is_shape_qualifying`), in the
///      same sorted order: a frame for its path, then a frame for its bytes.
///
/// Deliberately NOT hashed: the contents of ordinary source files (`.cc`,
/// `.F90`, …). Make's own `.d` dependency tracking is correct for body-code
/// edits, and hashing them would delete a thorn's whole build directory on
/// every edit — the exact "punish anyone iterating on sources" outcome
/// `SourceDelta::Edited`'s doc comment already rejects for the flesh; the
/// same trade applies per-thorn here.
///
/// Error semantics are load-bearing: if a thorn's directory cannot be read at
/// all — missing, or an I/O error anywhere inside it (listing it, reading a
/// qualifying file, hashing) — that thorn is simply omitted from the
/// returned map, never an error. This is fail-safe in both directions:
/// `shape_delta` treats stored-has-it/fresh-lacks-it as a change (a vanished
/// thorn's stale build state is invalidated), while a thorn absent on *both*
/// sides — the normal case in unit tests, and in a config whose fetch never
/// ran — produces no spurious delta. `thorn_shapes` therefore never returns a
/// `Result`: a build must never fail because a thorn directory happened to be
/// unreadable.
pub fn thorn_shapes(cactus_root: &Path, list: &Thornlist) -> BTreeMap<String, String> {
    let repos_root = fs::canonicalize(cactus_root.join("repos")).ok();
    let mut url_cache: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut out = BTreeMap::new();

    'thorn: for (name, provider) in list.thorn_providers() {
        let thorn_dir = cactus_root.join(&provider);

        let Ok(files) = shape_files(&thorn_dir) else { continue };
        let Ok(link_meta) = fs::symlink_metadata(&thorn_dir) else { continue };
        let shape_marker: Vec<u8> = if link_meta.file_type().is_symlink() {
            match fs::read_link(&thorn_dir) {
                Ok(target) => target.to_string_lossy().into_owned().into_bytes(),
                Err(_) => continue,
            }
        } else {
            b"<dir>".to_vec()
        };

        let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
        feed(&mut hasher, provider.as_bytes());
        feed(&mut hasher, &shape_marker);

        if let Some(repos_root) = &repos_root
            && let Ok(real) = fs::canonicalize(&thorn_dir)
            && let Some(repo_dir) = shape_repo_dir(&real, repos_root)
            && let Some(url) = shape_repo_url(&repo_dir, &mut url_cache)
        {
            feed(&mut hasher, url.as_bytes());
        }

        for f in &files {
            feed(&mut hasher, f.as_bytes());
        }
        for f in &files {
            let basename = Path::new(f).file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !is_shape_qualifying(basename) {
                continue;
            }
            let Ok(bytes) = fs::read(thorn_dir.join(f)) else { continue 'thorn };
            feed(&mut hasher, f.as_bytes());
            feed(&mut hasher, &bytes);
        }

        let Ok(id) = hasher.try_finalize() else { continue };
        out.insert(name, id.to_hex_with_len(16).to_string());
    }

    out
}

/// The `thorn_shapes` counterpart to `provider_delta`: names present in
/// `stored` whose `fresh` fingerprint differs or is absent. Semantics are
/// identical to `provider_delta` — read that doc comment for the reasoning —
/// mirrored exactly: names only in `fresh` are not a change (nothing built
/// yet to invalidate), and either side `None` means no information, never
/// "unchanged", so this returns empty rather than guessing.
pub fn shape_delta(
    stored: Option<&BTreeMap<String, String>>,
    fresh: Option<&BTreeMap<String, String>>,
) -> Vec<String> {
    let (Some(stored), Some(fresh)) = (stored, fresh) else {
        return Vec::new();
    };
    stored
        .iter()
        .filter(|(name, shape)| fresh.get(name.as_str()) != Some(shape))
        .map(|(name, _)| name.clone())
        .collect()
}

/// `stored_thornlist`/`fresh_thornlist` are the *processed* (toggles-applied)
/// texts, so this one comparison covers a source-thornlist edit, a switch to a
/// different thornlist file, and a change to the machine's or variant's
/// `enabled-thorns`/`disabled-thorns` — none of which the optionlist TOML diff
/// can see.
///
/// `sources` covers the case none of the text diffs can see at all: a refetch
/// that leaves the thornlist byte-identical and moves the repos underneath it.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_decision(
    stored_optionlist: Option<&str>,
    fresh_optionlist: &str,
    stored_universe: Option<&str>,
    resolved_universe: Option<&str>,
    stored_thornlist: Option<&str>,
    fresh_thornlist: &str,
    sources: SourceDelta,
    changed_providers: &[String],
    changed_shapes: &[String],
) -> RebuildDecision {
    match stored_optionlist {
        None => RebuildDecision::Fresh,
        Some(stored) if stored != fresh_optionlist => {
            RebuildDecision::Full("the optionlist changed")
        }
        Some(_) if stored_universe != resolved_universe => {
            RebuildDecision::Full("the build universe changed")
        }
        // Ranked above the thornlist diff because it is the strictly stronger
        // response: a release bump usually changes both at once.
        Some(_) if sources == SourceDelta::Flesh => {
            RebuildDecision::Full("the Cactus flesh moved to a different commit")
        }
        // An absent processed thornlist (hand-deleted from the config dir)
        // gives nothing to compare against, so it reads as unchanged; the
        // build rewrites it either way.
        Some(_) if stored_thornlist.is_some_and(|s| s != fresh_thornlist) => {
            RebuildDecision::Incremental("the thornlist changed")
        }
        // Normally a provider change implies the thornlist text changed too,
        // so the arm above already fired. This one catches the edge where the
        // processed thornlist was hand-deleted (the text diff above reads as
        // unchanged, since there's nothing to compare against) while the
        // provenance map still shows the swap.
        Some(_) if !changed_providers.is_empty() => {
            RebuildDecision::Incremental("thorn names changed provider")
        }
        // A thorn's content changed shape (a `.ccl` REQUIRES edit, a removed
        // source file, …) with the thornlist itself untouched — the gap
        // `thorn_providers`/`provider_delta` don't cover, since neither of
        // those changes moves which directory provides the name.
        Some(_) if !changed_shapes.is_empty() => {
            RebuildDecision::Incremental("thorn contents changed")
        }
        Some(_) if sources == SourceDelta::Thorns => {
            RebuildDecision::Incremental("the thorn sources moved to a different commit")
        }
        Some(_) if sources == SourceDelta::Edited => {
            RebuildDecision::Incremental("the source tree has local edits")
        }
        Some(_) => RebuildDecision::UpToDate,
    }
}

/// Resolve the BUILD universe name per §4.8 precedence (steps 1, 3, 4):
/// CLI → optionlist → [build].universe → declared host → None.
/// `--no-universe` stays the true bare escape hatch, bypassing even a
/// declared host; `host_declared` only matters as the final fallback, so
/// machines without `[universes.host]` keep resolving to None (implicit host
/// ≡ identity ≡ bare execution).
pub fn resolve_build_universe<'a>(
    opts: &'a BuildOpts,
    optionlist_universe: Option<&'a str>,
    machine_build_universe: Option<&'a str>,
    host_declared: bool,
) -> Option<&'a str> {
    if opts.universe.no_universe {
        return None;
    }
    opts.universe
        .universe
        .as_deref()
        .or(optionlist_universe)
        .or(machine_build_universe)
        .or(host_declared.then_some(crate::mdb::HOST_UNIVERSE))
}

pub struct BuildOutcome {
    pub meta: ConfigMeta,
    pub rebuilt: bool,
}

/// Join names for a one-line message, capping the tail: a release bump moves
/// every repo in the list, and 81 names is not a message.
fn summarize(names: &[String]) -> String {
    const SHOWN: usize = 8;
    let head = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    match names.len().checked_sub(SHOWN) {
        Some(rest) if rest > 0 => format!("{head}, +{rest} more"),
        _ => head,
    }
}

/// Run `config build` for `name` (§7). Returns the stored
/// metadata. The global DB is never touched here (§2.3); the caller updates
/// the active-config pointer afterwards.
pub fn build(
    installation: &Installation,
    machine: &Machine,
    name: &str,
    opts: &BuildOpts,
) -> Res<BuildOutcome> {
    let cactus_root = installation.cactus_root();
    if !cactus_root.is_dir() {
        bail!("no Cactus tree at {}", cactus_root.display());
    }
    let config_dir = cactus_root.join("configs").join(name);
    // Loaded up front: it carries the thornlist this config was last built
    // from, which feeds thornlist resolution below (§7.5).
    let stored_meta = ConfigMeta::load(&cactus_root, name)?;

    // Selection & inputs (§4.4, §7.8).
    let variant = machine.select_optionlist(opts.variant.as_deref())?;
    let optionlist = Optionlist::load(&machine.optionlist_path(&variant))?;
    let universe_name = resolve_build_universe(
        opts,
        optionlist.header.universe.as_deref(),
        machine.meta.build.universe.as_deref(),
        machine.meta.declared_host().is_some(),
    )
    .map(str::to_owned);
    // Unknown universe = hard error listing the known ones (§4.8).
    let universe = universe_name
        .as_deref()
        .map(|u| machine.meta.universe(u))
        .transpose()?;

    let thornlist =
        resolve_thornlist(&cactus_root, name, stored_meta.as_ref(), opts.thornlist.as_deref())?;
    if thornlist.from_snapshot {
        println!(
            "{} thornlist {} is no longer readable; building from the copy snapshotted in the \
             config ({}).",
            "warning:".yellow().bold(),
            thornlist.recorded,
            THORNLIST_SNAPSHOT,
        );
    }
    // Machine-level thorn toggles (§7.5) plus this optionlist variant's own
    // (§7.8): the variant augments the machine, so one machine can carry build
    // flavors that disable different thorns (e.g. a CUDA variant dropping
    // thorns that won't compile with nvcc). apply_thorn_toggles checks
    // `disabled` before `enabled`, so a variant disable wins over a machine
    // enable of the same thorn.
    let enabled_thorns: Vec<String> = machine
        .meta
        .build
        .enabled_thorns
        .iter()
        .chain(&optionlist.header.enabled_thorns)
        .cloned()
        .collect();
    let disabled_thorns: Vec<String> = machine
        .meta
        .build
        .disabled_thorns
        .iter()
        .chain(&optionlist.header.disabled_thorns)
        .cloned()
        .collect();
    let thornlist_processed =
        apply_thorn_toggles(&thornlist.text, &enabled_thorns, &disabled_thorns);

    if let Some(stored) = &stored_meta
        && stored.variant != variant
        && opts.variant.is_none()
    {
        bail!(
            "config \"{name}\" was built with variant \"{}\"; pass --variant explicitly to change it",
            stored.variant
        );
    }
    let flags = effective_flags(opts, stored_meta.as_ref().map(|m| m.flags));

    // Rebuild decision (§7.8): diff the SOURCE TOML snapshot, the universe, and
    // the processed thornlist.
    let snapshot_path = config_dir.join("cactup-optionlist.toml");
    let stored_optionlist = fs::read_to_string(&snapshot_path).ok();
    let stored_thornlist =
        fs::read_to_string(config_file(&cactus_root, name, THORNLIST_PROCESSED)).ok();
    // Parse the processed thornlist ONCE and derive both the live source
    // state and the per-thorn provenance map from it.
    //
    // Best-effort by design: a thornlist cactup cannot parse, or a tree with
    // no inspectable repo, yields `None` for either — which `source_delta`/
    // `provider_delta` read as "no information" and which therefore leaves
    // the rebuild decision exactly as it was before source/provenance
    // tracking. A build must never fail over this.
    let parsed_list = crate::thornlist::parse(&thornlist_processed).ok();
    // How the source trees now differ from what this config was built with
    // (§7.4) — a refetch, a manual checkout, or a hand-edited thorn. None of
    // the text diffs above can see any of it: they all leave the thornlist
    // byte-identical.
    let fresh_sources = parsed_list
        .as_ref()
        .and_then(|l| crate::fetch::source_heads(&installation.root, l).ok().flatten());
    let fresh_providers = parsed_list.as_ref().map(|l| l.thorn_providers());
    // Computed unconditionally, even under `-f`: the baseline must be
    // recorded on every build, or a config that always rebuilds with `-f`
    // could never acquire one to diff a later plain rebuild against.
    let fresh_shapes = parsed_list.as_ref().map(|l| thorn_shapes(&cactus_root, l));
    let (sources, source_change) =
        source_delta(stored_meta.as_ref().and_then(|m| m.sources.as_ref()), fresh_sources.as_ref());
    // Which thorn names changed which directory provides them (§7.4) — the
    // provider-swap incident this exists for: same name, different source
    // tree, and Cactus's per-thorn build state is keyed by name alone.
    let changed_providers = provider_delta(
        stored_meta.as_ref().and_then(|m| m.thorn_providers.as_ref()),
        fresh_providers.as_ref(),
    );
    // Which thorn names changed shape (§7.4) — the sibling gap: same name,
    // same provider, but different content (a `.ccl` edit, a removed source
    // file) that neither the thornlist text diff nor `provider_delta` sees.
    let changed_shapes = shape_delta(
        stored_meta.as_ref().and_then(|m| m.thorn_shapes.as_ref()),
        fresh_shapes.as_ref(),
    );
    let mut decision = rebuild_decision(
        stored_optionlist.as_deref(),
        &optionlist.source,
        stored_meta.as_ref().and_then(|m| m.universe.as_deref()),
        universe_name.as_deref(),
        stored_thornlist.as_deref(),
        &thornlist_processed,
        sources,
        &changed_providers,
        &changed_shapes,
    );
    if opts.force || opts.reconfig {
        decision = RebuildDecision::Full("-f/--reconfig given");
    } else if decision == RebuildDecision::UpToDate
        && is_complete(&cactus_root, name)
        && let Some(stored) = stored_meta.clone()
    {
        println!(
            "Config {} is up to date (same optionlist, same universe, same thornlist, same \
             sources); pass -f to rebuild.",
            name
        );
        // Record the source/provider/shape baseline even though nothing was
        // built. A config last built by a cactup without source, provenance,
        // or shape tracking has none of them, and without this it could never
        // acquire any: every future build would short-circuit here and the
        // next refetch, edit, provider swap, or content change would go
        // unnoticed. This writes metadata only — no build, and build-id/built
        // are preserved.
        let mut stored = stored;
        let sources_changed =
            fresh_sources.as_ref().is_some_and(|live| stored.sources.as_ref() != Some(&live.heads));
        let providers_changed =
            fresh_providers.as_ref().is_some_and(|live| stored.thorn_providers.as_ref() != Some(live));
        let shapes_changed =
            fresh_shapes.as_ref().is_some_and(|live| stored.thorn_shapes.as_ref() != Some(live));
        if sources_changed || providers_changed || shapes_changed {
            if let Some(live) = fresh_sources {
                stored.sources = Some(live.heads);
            }
            if let Some(live) = fresh_providers {
                stored.thorn_providers = Some(live);
            }
            if let Some(live) = fresh_shapes {
                stored.thorn_shapes = Some(live);
            }
            stored.store(&cactus_root)?;
        }
        return Ok(BuildOutcome { meta: stored, rebuilt: false });
    }
    // Say which cheaper path is being taken, so a thornlist edit does not look
    // like it was ignored (it used to be) and a *reconfigure* is not mistaken
    // for the from-scratch rebuild that `-f` gives.
    if let RebuildDecision::Incremental(why) = decision {
        println!(
            "Rebuilding config {name}: {why} — reconfiguring and rebuilding what that affects \
             (pass -f for a from-scratch rebuild)."
        );
    }
    if let RebuildDecision::Full("the Cactus flesh moved to a different commit") = decision {
        println!(
            "Rebuilding config {name} from scratch: the Cactus flesh moved, so the make system \
             and config-data are regenerated and every existing object is stale."
        );
    }
    // Name the repos, so a source-driven rebuild is actionable rather than
    // mysterious — especially the edited ones, which are the user's own work.
    if decision != RebuildDecision::UpToDate {
        if !source_change.moved.is_empty() {
            println!("  now on a different commit: {}", summarize(&source_change.moved));
        }
        if !source_change.edited.is_empty() {
            println!("  locally edited: {}", summarize(&source_change.edited));
        }
    }

    // Two incidents this exists for, both leaving a same-named build/<Thorn>/
    // holding state compiled from the wrong source: a provider swap (old
    // arrangement's stale `.d` files can name a bindings header the
    // reconfigure below is about to delete — a hard make error) and a shape
    // change (a `.ccl` REQUIRES edit or a removed source file, same failure
    // modes — see `thorn_shapes`'s doc comment). Either way Cactus updates an
    // existing libthorn_<Thorn>.a in place with `ar`, so stale members can
    // otherwise survive into the link without so much as a warning. Both
    // must go before make runs. `Full` is excluded on purpose: `realclean`
    // already wipes every config's build state, so this would just be
    // redundant there.
    let invalidated_thorns: Vec<String> = changed_providers
        .iter()
        .chain(&changed_shapes)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if matches!(decision, RebuildDecision::Incremental(_)) && !invalidated_thorns.is_empty() {
        fn remove_stale(path: &Path, remove: impl FnOnce(&Path) -> std::io::Result<()>) -> Res<()> {
            match remove(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| {
                    format!(
                        "Failed to remove stale per-thorn build state {} — proceeding would risk \
                         compiling or linking against a thorn's old provider",
                        path.display()
                    )
                }),
            }
        }
        if !changed_providers.is_empty() {
            println!(
                "  changed provider (removing their stale per-thorn build state): {}",
                summarize(&changed_providers)
            );
        }
        if !changed_shapes.is_empty() {
            println!(
                "  changed contents (removing their stale per-thorn build state): {}",
                summarize(&changed_shapes)
            );
        }
        for thorn in &invalidated_thorns {
            remove_stale(&config_dir.join("build").join(thorn), |p| fs::remove_dir_all(p))?;
            remove_stale(&config_dir.join("lib").join(format!("libthorn_{thorn}.a")), |p| fs::remove_file(p))?;
        }
    }

    // Build-context variables (§6.3, build-time set).
    let mut vars = VarSet::new();
    vars.set("MAKEJOBS", make_jobs_var(opts.make_jobs, machine.meta.build.make_jobs));
    vars.set("USER", std::env::var("USER").unwrap_or_default());
    vars.set("SOURCEDIR", cactus_root.display().to_string());
    vars.set("CONFIGURATION", name);
    // Resolve @USER@/@ENV()@ in scratch-home (§4.2) — the raw template would
    // otherwise leak `@USER@` literally, since substitution is single-pass and
    // never re-scans a spliced value. Matches the sim path (sim/vars.rs).
    vars.set(
        "SCRATCH_HOME",
        machine.meta.resolved_paths()?.scratch_home.unwrap_or_default(),
    );
    // Several machines' make commands / build universes reference
    // @ALLOCATION@ (e.g. mike's and Deep Bayou's `srun … singularity exec`
    // build wrappers); bind it from the allocation knob the way the sim path
    // does, empty when unset.
    let allocation = crate::database::Db::open()
        .and_then(|db| db.read())
        .map(|db| db.knob("allocation").unwrap_or("").to_owned())
        .unwrap_or_default();
    vars.set("ALLOCATION", allocation);

    // Rendered native optionlist: render → inject flags → substitute (§7.8).
    let rendered = vars
        .substitute(&inject_build_flags(&optionlist.render(), flags))
        .context("substituting the rendered optionlist")?;

    fs::create_dir_all(&config_dir)
        .with_context(|| format!("Failed to create {}", config_dir.display()))?;
    // Staging cactup's files makes configs/<name> exist before Cactus's
    // setup_configuration.pl ever runs, so it takes its "Reconfiguring"
    // branch — which chdirs into the skeleton only the new-config branch
    // creates. Create that skeleton ourselves, or the first configure dies
    // with "Internal error - couldn't enter '…/config-data'".
    for sub in ["build", "lib", "scratch", "config-data"] {
        fs::create_dir_all(config_dir.join(sub))
            .with_context(|| format!("Failed to create {}", config_dir.join(sub).display()))?;
    }
    let rendered_path = config_dir.join("cactup-optionlist.cfg");
    fs::write(&rendered_path, &rendered)?;
    let thornlist_out = config_dir.join(THORNLIST_PROCESSED);
    fs::write(&thornlist_out, &thornlist_processed)?;
    // Snapshot the source verbatim, so a rebuild survives the file it came from
    // moving or being deleted (resolve_thornlist step 3). Written from
    // `thornlist.text`, not the processed copy: a rebuild must re-apply
    // whatever the machine's thorn toggles say *then*, not replay old ones.
    let thornlist_snapshot = config_dir.join(THORNLIST_SNAPSHOT);
    fs::write(&thornlist_snapshot, &thornlist.text)
        .with_context(|| format!("Failed to write {}", thornlist_snapshot.display()))?;
    // Every build's combined output is teed here so a failure leaves something
    // to read once the terminal scrollback is gone (§7.2).
    let build_log = config_dir.join("cactup-build.log");

    // Per-config build lock, heartbeat-kept across the (long) make (§2.3 #4).
    let _build_lock = LinkLock::acquire(&config_dir.join(".cactup-build.lock"))?.with_heartbeat();

    if let Some(prebuilt) = &opts.virtual_executable {
        // §7.7: virtual/prebuilt executable — copy into place, skip make.
        let exe_dir = cactus_root.join("exe");
        fs::create_dir_all(&exe_dir)?;
        fs::copy(prebuilt, exe_dir.join(format!("cactus_{name}")))
            .with_context(|| format!("Failed to copy {}", prebuilt.display()))?;
    } else {
        // The default (`DEFAULT_MAKE`) templates @MAKEJOBS@ so
        // `[build].make-jobs` (§7.6: --make-jobs > machine make-jobs > 1) is
        // honored as the default -j even on machines that don't hand-write a
        // custom `make` key. A machine that sets its own `make` keeps full
        // control of parallelism.
        let make = vars
            .substitute(machine.meta.build.make.as_deref().unwrap_or(DEFAULT_MAKE))
            .context("substituting the machine make command")?;

        let mut steps: Vec<String> = Vec::new();
        if matches!(decision, RebuildDecision::Full(_)) && is_configured(&cactus_root, name) {
            steps.push(format!("{make} {name}-realclean"));
        }
        steps.push(format!(
            "echo yes | {make} {name}-config options={} THORNLIST={}",
            sh_quote(&rendered_path),
            sh_quote(&thornlist_out),
        ));
        if opts.clean {
            steps.push(format!("{make} {name}-clean"));
        }
        steps.push(format!("{make} {name}"));
        steps.push(format!("{make} {name}-utils"));

        // A wrapper universe may hand the build to the scheduler (e.g. an
        // srun prefix), which sits silently in the queue until it gets an
        // allocation — say so up front, or the wait looks like a hang.
        if let (Some(uname), Some(u)) = (universe_name.as_deref(), universe) {
            if u.wrapper.is_some() || u.wrapper_argv.is_some() {
                println!(
                    "Building inside universe \"{uname}\"; if its wrapper goes through the \
                     scheduler, output stays silent until the job is allocated (check the queue)."
                );
            }
        }

        // Build-phase env for the resolved universe (§6.1): universe env keys
        // override the machine [environment] key-by-key.
        let env = machine.meta.effective_env(universe_name.as_deref(), Phase::Build);
        let snippet = format!(
            "set -e\ncd {}\n{}{}",
            sh_quote(&cactus_root),
            if env.is_empty() { String::new() } else { format!("{env}\n") },
            steps.join("\n")
        );
        run_build_snippet(&snippet, universe, &vars, &build_log)?;
    }

    if !is_complete(&cactus_root, name) {
        // Report the component that is actually absent (§7.2). The two states
        // point the operator at opposite ends of the log:
        //   - configure never completed  → the marker is missing; look near
        //     the TOP of the log (a CST/configure error).
        //   - configure done, no exe     → the compile/link failed; look near
        //     the END of the log. run_build_snippet did NOT see a failure, so
        //     the build command reported success while the build failed —
        //     usually a scheduler wrapper swallowing the job's exit status.
        let (missing, hint) = if !is_configured(&cactus_root, name) {
            (
                completeness_marker(&cactus_root, name),
                "the configure step did not complete — look near the top of the build log",
            )
        } else {
            (
                executable_path(&cactus_root, name),
                "the compile/link step did not complete — look near the end of the build log; \
                 note the build command reported success, so if this machine's build universe \
                 goes through a scheduler its wrapper may be swallowing the job's exit status",
            )
        };
        let log_hint = if build_log.exists() {
            eprintln!(
                "\n{} the build command finished but {} is missing — the config is incomplete\n  {}\n{} {}",
                "✗".red().bold(),
                missing.display(),
                hint,
                "→ build log:".red().bold(),
                build_log.display(),
            );
            format!("; {hint}; see {}", build_log.display())
        } else {
            format!("; {hint}")
        };
        bail!(
            "the build command finished but {} is missing — the config is incomplete{log_hint}",
            missing.display(),
        );
    }

    // Metadata + rebuild snapshot (§7.4, §7.8).
    let now = Utc::now();
    let meta = ConfigMeta {
        schema: SCHEMA,
        name: name.to_owned(),
        variant: variant.clone(),
        gpu: optionlist.header.gpu,
        compatible_queues: optionlist.header.compatible_queues.clone(),
        thornlist: thornlist.recorded.clone(),
        machine: machine.name.clone(),
        universe: universe_name,
        coerce_run_universe: optionlist.header.coerce_run_universe,
        // config-id is stable across rebuilds; build-id is per-build (§7.4).
        config_id: stored_meta
            .map(|m| m.config_id)
            .unwrap_or_else(|| generate_id("config", name, &machine.name, now)),
        build_id: generate_id("build", name, &machine.name, now),
        built: Some(now),
        flags,
        sources: fresh_sources.map(|s| s.heads),
        thorn_providers: fresh_providers,
        thorn_shapes: fresh_shapes,
    };
    meta.store(&cactus_root)?;
    fs::write(&snapshot_path, &optionlist.source)
        .with_context(|| format!("Failed to write {}", snapshot_path.display()))?;

    Ok(BuildOutcome { meta, rebuilt: true })
}

/// `config-data/cctk_Config.h` presence ⇒ configured; + executable ⇒ complete
/// (§7.2).
fn is_configured(cactus_root: &Path, name: &str) -> bool {
    completeness_marker(cactus_root, name).is_file()
}

fn completeness_marker(cactus_root: &Path, name: &str) -> PathBuf {
    cactus_root
        .join("configs")
        .join(name)
        .join("config-data/cctk_Config.h")
}

pub fn executable_path(cactus_root: &Path, name: &str) -> PathBuf {
    cactus_root.join("exe").join(format!("cactus_{name}"))
}

pub fn is_complete(cactus_root: &Path, name: &str) -> bool {
    is_configured(cactus_root, name) && executable_path(cactus_root, name).is_file()
}

fn generate_id(kind: &str, name: &str, machine: &str, now: DateTime<Utc>) -> String {
    // A per-process counter disambiguates ids minted within the same second
    // (e.g. back-to-back rebuilds): the executable cache is keyed by build-id
    // (§8.1), so two distinct builds must never share one.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "{kind}-{name}-{machine}-{}-{}.{seq}",
        now.format("%Y.%m.%d-%H.%M.%S"),
        std::process::id()
    )
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Run the env-setup'd make snippet, wrapped in the build universe when one
/// is resolved (§7.2, §4.8). Build output streams to the terminal *and* is
/// teed (stdout+stderr, combined) to `log_path`, so a failed build leaves a
/// persistent record to read after the terminal scrollback is gone. On
/// failure we point the user at that log loudly, on stderr, before bailing.
fn run_build_snippet(
    snippet: &str,
    universe: Option<&crate::mdb::Universe>,
    vars: &VarSet,
    log_path: &Path,
) -> Res<()> {
    let mut cmd = match universe {
        None => {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", snippet]);
            c
        }
        Some(u) => match u.wrap(vars, snippet)? {
            crate::mdb::WrappedCommand::Shell(shell_cmd) => {
                let mut c = Command::new("/bin/sh");
                c.args(["-c", &shell_cmd]);
                c
            }
            crate::mdb::WrappedCommand::Argv(argv) => {
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
        },
    };
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::shell::trace_command(&cmd);

    // One combined log per build; File::create truncates any prior attempt.
    let log = Arc::new(Mutex::new(
        fs::File::create(log_path)
            .with_context(|| format!("Failed to create build log {}", log_path.display()))?,
    ));
    let mut child = cmd.spawn().context("Failed to spawn the build shell")?;

    // Mirror one child stream to a terminal fd and the shared log. stdout and
    // stderr keep their own destinations on-screen; both interleave into the
    // single log file (ordering approximate across the two streams, as in a
    // shell `2>&1`-style tee).
    fn tee(
        mut src: impl std::io::Read + Send + 'static,
        log: Arc<Mutex<fs::File>>,
        to_stderr: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match src.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if to_stderr {
                            let _ = std::io::stderr().write_all(chunk);
                        } else {
                            let _ = std::io::stdout().write_all(chunk);
                        }
                        if let Ok(mut f) = log.lock() {
                            let _ = f.write_all(chunk);
                        }
                    }
                }
            }
        })
    }

    let copiers = [
        tee(child.stdout.take().expect("stdout piped"), Arc::clone(&log), false),
        tee(child.stderr.take().expect("stderr piped"), Arc::clone(&log), true),
    ];
    let status = child.wait().context("Failed to wait on the build shell")?;
    for t in copiers {
        let _ = t.join();
    }

    if !status.success() {
        eprintln!(
            "\n{} the build failed ({status})\n{} {}",
            "✗".red().bold(),
            "→ build log:".red().bold(),
            log_path.display(),
        );
        bail!("the build failed ({status}); see {}", log_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdb::Mdb;

    /// A comment that merely mentions a thorn name must never be toggled —
    /// a custom thornlist's prose header documents thorns by name, and the
    /// native fetcher now copies that header into the live list verbatim.
    #[test]
    fn toggles_ignore_comment_lines_that_name_thorns() {
        let list = "# comment out CarpetX/TestReal2 below (#DISABLED CarpetX/TestReal2).\n\
                    #   CarpetX/TestReal2 carries the REAL2 layer\n\
                    CarpetX/TestReal2\n";
        let out = apply_thorn_toggles(&list, &["CarpetX/TestReal2".into()], &[]);
        assert_eq!(out, list, "enabling must not rewrite prose that names the thorn");
        let off = apply_thorn_toggles(&list, &[], &["CarpetX/TestReal2".into()]);
        assert_eq!(
            off,
            "# comment out CarpetX/TestReal2 below (#DISABLED CarpetX/TestReal2).\n\
             #   CarpetX/TestReal2 carries the REAL2 layer\n\
             #DISABLED CarpetX/TestReal2\n",
            "only the real thorn line is toggled"
        );
    }

    /// Both pre-rename escapes in thornlist resolution: rule 4 finds a live
    /// list still under the old name, and rule 2 follows a recorded old-name
    /// path to the renamed file beside it rather than dropping to the snapshot
    /// (which would stop picking up edits to the live list).
    #[test]
    fn resolve_thornlist_tolerates_the_pre_rename_name() {
        let dir = tempfile::tempdir().unwrap();
        let cactus = dir.path().join("Cactus");
        let lists = cactus.join("thornlists");
        fs::create_dir_all(&lists).unwrap();
        let legacy = lists.join(crate::installation::LEGACY_THORNLIST);
        fs::write(&legacy, "A/B\n").unwrap();

        // Rule 4, un-migrated: the old name is the only live list there is.
        assert_eq!(default_thornlist(&cactus), legacy);
        let fresh = resolve_thornlist(&cactus, "sim", None, None).unwrap();
        assert_eq!(fresh.text, "A/B\n");
        assert_eq!(fresh.recorded, legacy.display().to_string());

        // Rule 4, migrated: the current name wins even with the old one left
        // behind.
        let current = lists.join(crate::installation::LIVE_THORNLIST);
        fs::write(&current, "A/B\nC/D\n").unwrap();
        assert_eq!(default_thornlist(&cactus), current);

        // Rule 2 with a recorded path whose old-name file is gone.
        fs::remove_file(&legacy).unwrap();
        let stored = ConfigMeta {
            schema: SCHEMA,
            name: "sim".to_owned(),
            variant: "default".to_owned(),
            gpu: false,
            compatible_queues: Vec::new(),
            thornlist: legacy.display().to_string(),
            machine: "fake".to_owned(),
            universe: None,
            coerce_run_universe: true,
            config_id: "c1".to_owned(),
            build_id: "b1".to_owned(),
            built: None,
            flags: BuildFlags::default(),
            sources: None,
            thorn_providers: None,
            thorn_shapes: None,
        };
        let resolved = resolve_thornlist(&cactus, "sim", Some(&stored), None).unwrap();
        assert!(!resolved.from_snapshot, "the renamed live list, not the snapshot");
        assert_eq!(resolved.text, "A/B\nC/D\n");
        assert_eq!(resolved.recorded, current.display().to_string());
    }

    #[test]
    fn thorn_toggles() {
        let list = "# comment\nCactusBase/IOUtil\n#DISABLED McLachlan/ML_BSSN\nCarpetX/CarpetX\n";
        let out = apply_thorn_toggles(
            list,
            &["ML_BSSN".to_owned()],
            &["CarpetX/CarpetX".to_owned()],
        );
        assert_eq!(
            out,
            "# comment\nCactusBase/IOUtil\nMcLachlan/ML_BSSN\n#DISABLED CarpetX/CarpetX\n"
        );
        // Toggles are idempotent.
        assert_eq!(
            apply_thorn_toggles(&out, &["ML_BSSN".into()], &["CarpetX/CarpetX".into()]),
            out
        );
    }

    #[test]
    fn flag_injection_replaces_or_appends() {
        let rendered = "VERSION = 2020\nDEBUG = yes\nCC = gcc\n";
        let out = inject_build_flags(rendered, BuildFlags::default());
        assert!(out.contains("DEBUG = no"), "{out}");
        assert!(out.contains("OPTIMISE = yes"), "{out}");
        assert!(out.contains("UNSAFE = no") && out.contains("PROFILE = no"));
        assert!(out.starts_with("VERSION = 2020\n"), "order preserved: {out}");
        assert_eq!(out.matches("DEBUG").count(), 1, "replaced, not duplicated");
    }

    #[test]
    fn default_make_honors_make_jobs() {
        // When a machine omits `[build].make`, the default templates @MAKEJOBS@
        // so the resolved -j tracks `[build].make-jobs` (here, 6).
        let mut vars = VarSet::new();
        vars.set("MAKEJOBS", 6u64);
        assert_eq!(vars.substitute(DEFAULT_MAKE).unwrap(), "make -j6");
    }

    #[test]
    fn make_jobs_precedence_and_max() {
        // --make-jobs wins over the machine default.
        assert_eq!(make_jobs_var(Some(MakeJobs::Count(12)), Some(4)), VarValue::Int(12));
        // Falls back to the machine make-jobs, then to 1.
        assert_eq!(make_jobs_var(None, Some(4)), VarValue::Int(4));
        assert_eq!(make_jobs_var(None, None), VarValue::Int(1));
        // `-j max` resolves to a runtime nproc, so the build shell counts the
        // CPUs available in whatever context it runs in.
        assert_eq!(make_jobs_var(Some(MakeJobs::Max), Some(4)), VarValue::Str(MAX_MAKEJOBS.into()));

        // Substituted into the default make command it yields a live expansion.
        let mut vars = VarSet::new();
        vars.set("MAKEJOBS", make_jobs_var(Some(MakeJobs::Max), None));
        assert_eq!(
            vars.substitute(DEFAULT_MAKE).unwrap(),
            "make -j$(nproc 2>/dev/null || echo 1)"
        );
    }

    #[test]
    fn build_universe_precedence_and_host_fallback() {
        let mut opts = BuildOpts::default_for_tests();
        // §4.8: CLI → optionlist → [build].universe → declared host → None.
        assert_eq!(resolve_build_universe(&opts, None, None, false), None);
        assert_eq!(resolve_build_universe(&opts, None, None, true), Some("host"));
        assert_eq!(resolve_build_universe(&opts, None, Some("m"), true), Some("m"));
        assert_eq!(resolve_build_universe(&opts, Some("o"), Some("m"), true), Some("o"));
        opts.universe.universe = Some("cli".to_owned());
        assert_eq!(resolve_build_universe(&opts, Some("o"), Some("m"), true), Some("cli"));
        // --no-universe is the true bare escape hatch: it bypasses everything,
        // including a declared host.
        opts.universe.no_universe = true;
        assert_eq!(resolve_build_universe(&opts, Some("o"), Some("m"), true), None);
    }

    #[test]
    fn rebuild_decisions() {
        use RebuildDecision as R;
        use SourceDelta as S;
        // Nothing refetched under the config: the pre-source-tracking matrix,
        // which must be unchanged.
        let n = S::Unchanged;
        let no_prov: &[String] = &[];
        let no_shapes: &[String] = &[];
        let swapped: &[String] = &["WeylScal4".to_owned()];
        let reshaped: &[String] = &["WeylScal4".to_owned()];
        // No stored optionlist at all ⇒ nothing has been built here yet.
        assert_eq!(rebuild_decision(None, "x", None, None, None, "t", n, no_prov, no_shapes), R::Fresh);
        assert_eq!(
            rebuild_decision(Some("x"), "x", None, None, Some("t"), "t", n, no_prov, no_shapes),
            R::UpToDate
        );
        assert!(matches!(
            rebuild_decision(Some("x"), "y", None, None, Some("t"), "t", n, no_prov, no_shapes),
            R::Full(_)
        ));
        assert!(matches!(
            rebuild_decision(Some("x"), "x", Some("et-sif"), None, Some("t"), "t", n, no_prov, no_shapes),
            R::Full(_)
        ));
        assert_eq!(
            rebuild_decision(Some("x"), "x", Some("u"), Some("u"), Some("t"), "t", n, no_prov, no_shapes),
            R::UpToDate
        );

        // A thornlist edit is a rebuild — the bug this fixes was it reading as
        // up-to-date — but a reconfigure, not a realclean.
        assert!(matches!(
            rebuild_decision(Some("x"), "x", None, None, Some("t"), "t2", n, no_prov, no_shapes),
            R::Incremental(_)
        ));
        // An optionlist change outranks it: realclean wins over reconfigure.
        assert!(matches!(
            rebuild_decision(Some("x"), "y", None, None, Some("t"), "t2", n, no_prov, no_shapes),
            R::Full(_)
        ));
        // No processed thornlist on disk ⇒ nothing to compare, not an edit.
        assert_eq!(
            rebuild_decision(Some("x"), "x", None, None, None, "t", n, no_prov, no_shapes),
            R::UpToDate
        );

        // Source tracking: a refetch with an untouched thornlist used to read
        // as UpToDate and silently never compile the new sources.
        assert!(matches!(
            rebuild_decision(Some("x"), "x", None, None, Some("t"), "t", S::Thorns, no_prov, no_shapes),
            R::Incremental(_)
        ));
        // The flesh earns a realclean, and outranks a simultaneous thornlist
        // edit — a release bump changes both at once.
        assert!(matches!(
            rebuild_decision(Some("x"), "x", None, None, Some("t"), "t", S::Flesh, no_prov, no_shapes),
            R::Full(_)
        ));
        assert!(matches!(
            rebuild_decision(Some("x"), "x", None, None, Some("t"), "t2", S::Flesh, no_prov, no_shapes),
            R::Full(_)
        ));
        // No fetch record / unparseable thornlist ⇒ exactly the old behavior.
        assert_eq!(
            rebuild_decision(Some("x"), "x", None, None, Some("t"), "t", S::Unknown, no_prov, no_shapes),
            R::UpToDate
        );

        // A provider swap with byte-identical thornlist text (the edge case
        // this exists for: the processed thornlist was hand-deleted, so the
        // text diff reads as unchanged) still triggers a reconfigure.
        assert!(matches!(
            rebuild_decision(Some("x"), "x", None, None, Some("t"), "t", n, swapped, no_shapes),
            R::Incremental(_)
        ));
        assert!(matches!(
            rebuild_decision(Some("x"), "x", None, None, None, "t", n, swapped, no_shapes),
            R::Incremental(_)
        ));
        // An optionlist change still outranks a provider swap: realclean wins.
        assert!(matches!(
            rebuild_decision(Some("x"), "y", None, None, Some("t"), "t", n, swapped, no_shapes),
            R::Full(_)
        ));

        // A shape-only change (content, not provider) also triggers a
        // reconfigure — the gap this whole mechanism exists for.
        assert!(matches!(
            rebuild_decision(Some("x"), "x", None, None, Some("t"), "t", n, no_prov, reshaped),
            R::Incremental(_)
        ));
        // ...even with the processed thornlist hand-deleted, same as the
        // provider-swap edge case above.
        assert!(matches!(
            rebuild_decision(Some("x"), "x", None, None, None, "t", n, no_prov, reshaped),
            R::Incremental(_)
        ));
        // An optionlist change still outranks a shape change: realclean wins.
        assert!(matches!(
            rebuild_decision(Some("x"), "y", None, None, Some("t"), "t", n, no_prov, reshaped),
            R::Full(_)
        ));
    }

    fn heads(pairs: &[(&str, &str)], flesh: Option<&str>) -> SourceHeads {
        SourceHeads {
            heads: pairs.iter().map(|(r, h)| (r.to_string(), h.to_string())).collect(),
            dirty: pairs
                .iter()
                .filter(|(_, h)| h.contains('+'))
                .map(|(r, _)| r.to_string())
                .collect(),
            flesh: flesh.map(str::to_owned),
        }
    }

    #[test]
    fn source_deltas() {
        let stored: BTreeMap<String, String> = [("cactusbase", "aaa"), ("flesh", "fff")]
            .iter()
            .map(|(r, h)| (r.to_string(), h.to_string()))
            .collect();

        // Either side absent ⇒ no information, never "unchanged".
        assert_eq!(
            source_delta(None, Some(&heads(&[("cactusbase", "bbb")], None))).0,
            SourceDelta::Unknown
        );
        assert_eq!(source_delta(Some(&stored), None).0, SourceDelta::Unknown);

        let same = heads(&[("cactusbase", "aaa"), ("flesh", "fff")], Some("flesh"));
        assert_eq!(source_delta(Some(&stored), Some(&same)).0, SourceDelta::Unchanged);

        // A thorn repo moved to another commit.
        let moved = heads(&[("cactusbase", "bbb"), ("flesh", "fff")], Some("flesh"));
        let (delta, change) = source_delta(Some(&stored), Some(&moved));
        assert_eq!(delta, SourceDelta::Thorns);
        assert_eq!(change.moved, vec!["cactusbase".to_string()]);
        assert!(change.edited.is_empty());

        // The flesh moved: outranks the thorns that moved alongside it.
        let release_bump = heads(&[("cactusbase", "bbb"), ("flesh", "ggg")], Some("flesh"));
        let (delta, change) = source_delta(Some(&stored), Some(&release_bump));
        assert_eq!(delta, SourceDelta::Flesh);
        assert_eq!(change.moved, vec!["cactusbase".to_string(), "flesh".to_string()]);

        // A hand-edited thorn: same commit, different worktree. This is the
        // edit-in-place workflow, and it must rebuild.
        let edited = heads(&[("cactusbase", "aaa+1mod@99"), ("flesh", "fff")], Some("flesh"));
        let (delta, change) = source_delta(Some(&stored), Some(&edited));
        assert_eq!(delta, SourceDelta::Edited);
        assert_eq!(change.edited, vec!["cactusbase".to_string()]);
        assert!(change.moved.is_empty());

        // Editing a *second* file must still register against a build made
        // with the first already edited — the mtime/count summary is what
        // makes repeated edits distinguishable.
        let one_edit: BTreeMap<String, String> =
            [("cactusbase", "aaa+1mod@99")].iter().map(|(r, h)| (r.to_string(), h.to_string())).collect();
        let two_edits = heads(&[("cactusbase", "aaa+2mod@120")], None);
        assert_eq!(source_delta(Some(&one_edit), Some(&two_edits)).0, SourceDelta::Edited);
        // …and reverting back to clean is also a change.
        let reverted = heads(&[("cactusbase", "aaa")], None);
        assert_eq!(source_delta(Some(&one_edit), Some(&reverted)).0, SourceDelta::Edited);

        // Editing the FLESH in place stays incremental: make recompiles what
        // the edit affects, and a realclean would punish iterating on it.
        let flesh_edit = heads(&[("cactusbase", "aaa"), ("flesh", "fff+1mod@99")], Some("flesh"));
        assert_eq!(source_delta(Some(&stored), Some(&flesh_edit)).0, SourceDelta::Edited);

        // A moved commit outranks a hand edit elsewhere.
        let both = heads(&[("cactusbase", "bbb"), ("flesh", "fff+1mod@99")], Some("flesh"));
        let (delta, change) = source_delta(Some(&stored), Some(&both));
        assert_eq!(delta, SourceDelta::Thorns);
        assert_eq!(change.moved, vec!["cactusbase".to_string()]);
        assert_eq!(change.edited, vec!["flesh".to_string()]);

        // A repo the stored record never knew about is NOT a change: the first
        // build after source tracking landed would otherwise realclean.
        let newly_tracked =
            heads(&[("cactusbase", "aaa"), ("flesh", "fff"), ("llama", "zzz")], Some("flesh"));
        assert_eq!(source_delta(Some(&stored), Some(&newly_tracked)).0, SourceDelta::Unchanged);

        // A flesh repo that moved but is not identified as the flesh stays
        // Incremental — is_flesh is what promotes it.
        let unnamed = heads(&[("cactusbase", "aaa"), ("flesh", "ggg")], None);
        assert_eq!(source_delta(Some(&stored), Some(&unnamed)).0, SourceDelta::Thorns);
    }

    fn provider_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn provider_deltas() {
        let stored = provider_map(&[
            ("WeylScal4", "arrangements/EinsteinAnalysis/WeylScal4"),
            ("Boundary", "arrangements/CactusBase/Boundary"),
        ]);

        // Either side (or both) absent ⇒ no information, never "unchanged".
        assert!(provider_delta(None, None).is_empty());
        assert!(provider_delta(Some(&stored), None).is_empty());
        assert!(provider_delta(None, Some(&stored)).is_empty());

        // Identical ⇒ nothing to invalidate.
        assert!(provider_delta(Some(&stored), Some(&stored)).is_empty());

        // A swapped provider is reported.
        let swapped = provider_map(&[
            ("WeylScal4", "arrangements/SpacetimeX/WeylScal4"),
            ("Boundary", "arrangements/CactusBase/Boundary"),
        ]);
        assert_eq!(provider_delta(Some(&stored), Some(&swapped)), vec!["WeylScal4".to_string()]);

        // A dropped thorn is reported too — its stale build state must go, or
        // re-adding the name later from a different provider would find no
        // baseline to diff against.
        let dropped = provider_map(&[("Boundary", "arrangements/CactusBase/Boundary")]);
        assert_eq!(provider_delta(Some(&stored), Some(&dropped)), vec!["WeylScal4".to_string()]);

        // An added-only thorn is not a change: nothing exists yet to
        // invalidate.
        let added = provider_map(&[
            ("WeylScal4", "arrangements/EinsteinAnalysis/WeylScal4"),
            ("Boundary", "arrangements/CactusBase/Boundary"),
            ("ML_BSSN", "arrangements/McLachlan/ML_BSSN"),
        ]);
        assert!(provider_delta(Some(&stored), Some(&added)).is_empty());
    }

    #[test]
    fn shape_deltas() {
        let stored = provider_map(&[("WeylScal4", "aaa"), ("Boundary", "bbb")]);

        // Either side (or both) absent ⇒ no information, never "unchanged".
        assert!(shape_delta(None, None).is_empty());
        assert!(shape_delta(Some(&stored), None).is_empty());
        assert!(shape_delta(None, Some(&stored)).is_empty());

        // Identical ⇒ nothing to invalidate.
        assert!(shape_delta(Some(&stored), Some(&stored)).is_empty());

        // A changed fingerprint is reported.
        let changed = provider_map(&[("WeylScal4", "ccc"), ("Boundary", "bbb")]);
        assert_eq!(shape_delta(Some(&stored), Some(&changed)), vec!["WeylScal4".to_string()]);

        // A dropped thorn is reported too — same reasoning as `provider_delta`:
        // re-adding the name later would otherwise find no baseline to diff.
        let dropped = provider_map(&[("Boundary", "bbb")]);
        assert_eq!(shape_delta(Some(&stored), Some(&dropped)), vec!["WeylScal4".to_string()]);

        // An added-only thorn is not a change: nothing exists yet to
        // invalidate.
        let added = provider_map(&[("WeylScal4", "aaa"), ("Boundary", "bbb"), ("ML_BSSN", "ddd")]);
        assert!(shape_delta(Some(&stored), Some(&added)).is_empty());
    }

    #[test]
    fn summarize_caps_the_tail() {
        let few: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(summarize(&few), "a, b");
        let many: Vec<String> = (0..11).map(|i| format!("r{i}")).collect();
        assert_eq!(summarize(&many), "r0, r1, r2, r3, r4, r5, r6, r7, +3 more");
    }

    /// End-to-end against a fake Cactus tree whose machine `make` is a shell
    /// function-free stub script that records its invocations and fabricates
    /// the completeness markers.
    #[test]
    fn build_drives_make_and_writes_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\nC/D\n").unwrap();

        // Fake make: log every call; on `<name>-config` / `<name>` fabricate
        // the marker / executable. The `-config` step emulates the crucial
        // setup_configuration.pl behavior: configs/sim already exists (cactup
        // staged files there), so its "Reconfiguring" branch runs, which
        // requires the config-data skeleton to exist already — it chdirs
        // instead of creating it.
        let fake_make = root.join("fakemake");
        fs::write(
            &fake_make,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}/make.log\necho \"fake-make: $@\"\ncase \"$2\" in\n\
                 sim-config) cd {}/configs/sim/config-data || \
                 {{ echo \"Internal error - couldn't enter config-data\"; exit 1; }}; \
                 touch cctk_Config.h ;;\n\
                 sim) mkdir -p {}/exe && touch {}/exe/cactus_sim ;;\nesac\n",
                root.display(), cactus.display(), cactus.display(), cactus.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_make, fs::Permissions::from_mode(0o755)).unwrap();
        }

        // A machine whose make points at the stub and which disables C/D.
        let machine_dir = root.join("mdb/fake");
        fs::create_dir_all(machine_dir.join("optionlists")).unwrap();
        fs::create_dir_all(machine_dir.join("runscripts")).unwrap();
        fs::create_dir_all(machine_dir.join("submitscripts")).unwrap();
        fs::write(
            machine_dir.join("meta.toml"),
            format!(
                r#"
                [machine]
                nickname = "fake"
                [build]
                make = "{} -j@MAKEJOBS@"
                make-jobs = 4
                disabled-thorns = ["C/D"]
                [environment]
                env-setup = "CACTUP_BUILD_ENV=on"
                [queues.local]
                default = true
                [variants.submitscript]
                "default" = ["local"]
                [variants.runscript]
                "default" = ["local"]
                [variants.optionlist]
                variants = ["default"]
                "#,
                fake_make.display()
            ),
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n[options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        for s in ["runscripts/default.sh", "submitscripts/default.sh"] {
            fs::write(machine_dir.join(s), "#!/bin/sh\n").unwrap();
        }

        let mdb = Mdb::with_roots(root.join("mdb"), PathBuf::from("/nonexistent"));
        let machine = mdb.load("fake").unwrap();
        let inst = Installation::new("et", root.join("inst"));
        let opts = BuildOpts::default_for_tests();

        let outcome = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(outcome.rebuilt);
        let meta = &outcome.meta;
        assert_eq!(meta.variant, "default");
        assert_eq!(meta.compatible_queues, ["local"]);
        assert_eq!(meta.machine, "fake");
        assert!(meta.universe.is_none() && meta.coerce_run_universe);
        assert!(meta.flags.optimize && !meta.flags.debug);

        // make was driven with -j4 (machine make-jobs), config→build→utils.
        let log = fs::read_to_string(root.join("make.log")).unwrap();
        assert!(log.contains("-j4 sim-config options="), "{log}");
        assert!(log.contains("-j4 sim\n"), "{log}");
        assert!(log.contains("-j4 sim-utils"), "{log}");
        assert!(!log.contains("realclean"), "fresh build must not realclean: {log}");

        // Thorn toggle applied; flags injected into the rendered optionlist.
        let thornlist = fs::read_to_string(cactus.join("configs/sim/cactup-thornlist.th")).unwrap();
        assert!(thornlist.contains("#DISABLED C/D"), "{thornlist}");
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.starts_with("VERSION = 1\n") && rendered.contains("OPTIMISE = yes"));

        // Combined build output was teed to the per-config log.
        let build_log =
            fs::read_to_string(cactus.join("configs/sim/cactup-build.log")).unwrap();
        assert!(build_log.contains("fake-make: -j4 sim-config"), "{build_log}");
        assert!(build_log.contains("fake-make: -j4 sim\n"), "{build_log}");

        // Second build with nothing changed: up-to-date short-circuit.
        let again = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(!again.rebuilt);
        assert_eq!(again.meta.build_id, outcome.meta.build_id);

        // Changed optionlist ⇒ full rebuild (realclean) + new build-id,
        // stable config-id (§7.4, §7.8).
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n[options]\nVERSION = \"2\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        let machine = mdb.load("fake").unwrap();
        let rebuilt = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(rebuilt.rebuilt);
        assert_ne!(rebuilt.meta.build_id, outcome.meta.build_id);
        assert_eq!(rebuilt.meta.config_id, outcome.meta.config_id);
        let log = fs::read_to_string(root.join("make.log")).unwrap();
        assert!(log.contains("sim-realclean"), "{log}");
    }

    /// A failing `make` must still leave a readable build log, and the error
    /// must point the user at it.
    #[test]
    fn failed_build_writes_log_and_points_at_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();

        // Fake make: emit a diagnostic and fail on the compile step (`sim`),
        // after the config step fabricated the marker.
        let fake_make = root.join("fakemake");
        fs::write(
            &fake_make,
            format!(
                "#!/bin/sh\ncase \"$2\" in\n\
                 sim-config) cd {}/configs/sim/config-data && touch cctk_Config.h ;;\n\
                 sim) echo 'gcc: fatal error: no input files' 1>&2; exit 2 ;;\nesac\n",
                cactus.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_make, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let machine_dir = root.join("mdb/fake");
        fs::create_dir_all(machine_dir.join("optionlists")).unwrap();
        fs::create_dir_all(machine_dir.join("runscripts")).unwrap();
        fs::create_dir_all(machine_dir.join("submitscripts")).unwrap();
        fs::write(
            machine_dir.join("meta.toml"),
            format!(
                r#"
                [machine]
                nickname = "fake"
                [build]
                make = "{} -j@MAKEJOBS@"
                [queues.local]
                default = true
                [variants.submitscript]
                "default" = ["local"]
                [variants.runscript]
                "default" = ["local"]
                [variants.optionlist]
                variants = ["default"]
                "#,
                fake_make.display()
            ),
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n[options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        for s in ["runscripts/default.sh", "submitscripts/default.sh"] {
            fs::write(machine_dir.join(s), "#!/bin/sh\n").unwrap();
        }

        let mdb = Mdb::with_roots(root.join("mdb"), PathBuf::from("/nonexistent"));
        let machine = mdb.load("fake").unwrap();
        let inst = Installation::new("et", root.join("inst"));
        let opts = BuildOpts::default_for_tests();

        let err = match build(&inst, &machine, "sim", &opts) {
            Ok(_) => panic!("build should have failed"),
            Err(e) => e,
        };
        let build_log = cactus.join("configs/sim/cactup-build.log");
        // The error names the log path...
        assert!(
            err.to_string().contains(&build_log.display().to_string()),
            "error should point at the log: {err}"
        );
        // ...and the log captured make's stderr diagnostic.
        let captured = fs::read_to_string(&build_log).unwrap();
        assert!(captured.contains("no input files"), "{captured}");
    }

    /// Stage a fake Cactus tree whose machine `make` is `make_body` (a `case
    /// "$2" in … esac` over the make target). Returns the pieces `build()`
    /// needs. Shared by the incompleteness-message tests below.
    fn fake_tree(root: &Path, make_body: &str) -> (Mdb, Machine, Installation, BuildOpts) {
        let cactus = root.join("inst/Cactus");
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();

        let fake_make = root.join("fakemake");
        fs::write(&fake_make, format!("#!/bin/sh\ncase \"$2\" in\n{make_body}\nesac\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_make, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let machine_dir = root.join("mdb/fake");
        fs::create_dir_all(machine_dir.join("optionlists")).unwrap();
        fs::create_dir_all(machine_dir.join("runscripts")).unwrap();
        fs::create_dir_all(machine_dir.join("submitscripts")).unwrap();
        fs::write(
            machine_dir.join("meta.toml"),
            format!(
                r#"
                [machine]
                nickname = "fake"
                [build]
                make = "{} -j@MAKEJOBS@"
                [queues.local]
                default = true
                [variants.submitscript]
                "default" = ["local"]
                [variants.runscript]
                "default" = ["local"]
                [variants.optionlist]
                variants = ["default"]
                "#,
                fake_make.display()
            ),
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n[options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        for s in ["runscripts/default.sh", "submitscripts/default.sh"] {
            fs::write(machine_dir.join(s), "#!/bin/sh\n").unwrap();
        }

        let mdb = Mdb::with_roots(root.join("mdb"), PathBuf::from("/nonexistent"));
        let machine = mdb.load("fake").unwrap();
        let inst = Installation::new("et", root.join("inst"));
        (mdb, machine, inst, BuildOpts::default_for_tests())
    }

    /// Defect A, compile-failure branch: configure produced the marker but the
    /// make exited 0 without producing the executable (a scheduler wrapper
    /// swallowed the inner make's real exit status). The incompleteness error
    /// must name the missing *executable* and flag the swallowed-status case —
    /// not blame the configure marker, which is present.
    #[test]
    fn incomplete_after_configure_names_executable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        // config step fabricates the marker; compile step prints a lua-shim
        // banner and "succeeds" without ever linking the exe.
        let (_mdb, machine, inst, opts) = fake_tree(
            root,
            &format!(
                "sim-config) cd {}/configs/sim/config-data && touch cctk_Config.h ;;\n\
                 sim) echo 'sbatch: lua: Submitted job 999'; exit 0 ;;",
                cactus.display()
            ),
        );

        let err = match build(&inst, &machine, "sim", &opts) {
            Ok(_) => panic!("build should have failed as incomplete"),
            Err(e) => e.to_string(),
        };
        let exe = executable_path(&cactus, "sim");
        assert!(err.contains(&exe.display().to_string()), "should name the exe: {err}");
        assert!(!err.contains("cctk_Config.h"), "must not blame the configure marker: {err}");
        assert!(err.contains("exit status"), "should flag the swallowed-status case: {err}");
    }

    /// The thornlist is a real rebuild input (§7.8), is remembered across
    /// rebuilds, and is snapshotted so a config survives its source file going
    /// away (§7.5). Previously an edited thornlist read as "up to date" and a
    /// rebuild without `--thornlist` silently reverted to the live list.
    /// End-to-end for the §7.4 source-tracking hookup: a refetch that leaves
    /// the thornlist byte-identical must still rebuild, and moving the flesh
    /// must escalate that to a realclean. Before this wiring, every case here
    /// short-circuited as "up to date" and silently never compiled the new
    /// sources.
    #[test]
    fn source_tree_changes_are_a_rebuild_input() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, opts) = fake_tree(
            root,
            &format!(
                "sim-config) echo \"$@\" >> {r}/make.log; cd {c}/configs/sim/config-data && \
                 touch cctk_Config.h ;;\n\
                 sim) echo \"$@\" >> {r}/make.log; mkdir -p {c}/exe && \
                 touch {c}/exe/cactus_sim ;;\n\
                 *) echo \"$@\" >> {r}/make.log ;;",
                r = root.display(),
                c = cactus.display()
            ),
        );
        let log = || fs::read_to_string(root.join("make.log")).unwrap_or_default();
        let clear_log = || {
            let _ = fs::remove_file(root.join("make.log"));
        };

        // A real CRL list, so the processed copy parses and maps thorns→repos.
        // `core` is the flesh: it checks Makefile/lib/src into the Cactus root.
        let list = root.join("crl.th");
        fs::write(
            &list,
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET   = $ROOT\n!TYPE = git\n!URL = https://e.invalid/cactus.git\n\
             !NAME = core\n!CHECKOUT = Makefile lib src\n\n\
             !TARGET   = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://e.invalid/cactusbase.git\n\
             !CHECKOUT = CactusBase/Boundary\n",
        )
        .unwrap();

        // Real repos, because source tracking reads the live tree — not a
        // replay of fetch-state.toml. Editing a thorn in place and rebuilding
        // is an ordinary workflow and must be caught the same way a refetch is.
        let repos = cactus.join("repos");
        for name in ["core", "cactusbase"] {
            crate::fetch::git::testrepo::init(&repos.join(name));
            crate::fetch::git::testrepo::commit_file(&repos.join(name), "thorn.cc", "int a;\n");
        }

        let mut first_opts = BuildOpts::default_for_tests();
        first_opts.thornlist = Some(list.clone());
        let first = build(&inst, &machine, "sim", &first_opts).unwrap();
        assert!(first.rebuilt);
        // The live state is recorded, keyed by repo, only for repos this list
        // names.
        let recorded = first.meta.sources.clone().expect("sources must be recorded");
        assert_eq!(recorded.len(), 2, "core + cactusbase: {recorded:?}");
        assert!(recorded.contains_key("core") && recorded.contains_key("cactusbase"));

        // Nothing moved ⇒ still short-circuits.
        let again = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(!again.rebuilt, "nothing moved, so it must short-circuit");

        // A thorn repo moves to a new commit (a refetch, or a manual checkout):
        // rebuild and reconfigure, but no realclean. The thornlist file is
        // untouched — exactly the case the text diffs cannot see.
        clear_log();
        crate::fetch::git::testrepo::commit(&repos.join("cactusbase"), "new thorn work");
        let thorns = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(thorns.rebuilt, "a moved thorn repo must rebuild");
        assert!(log().contains("sim-config"), "must reconfigure: {}", log());
        assert!(!log().contains("realclean"), "a thorn source change needs no realclean: {}", log());
        // The new state is recorded, so the next build short-circuits again.
        let settled = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(!settled.rebuilt, "the new state must become the baseline");

        // The flesh moves: realclean, because config-data and every object
        // built against the old make system are stale.
        clear_log();
        crate::fetch::git::testrepo::commit(&repos.join("core"), "flesh work");
        let flesh = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(flesh.rebuilt, "a moved flesh must rebuild");
        assert!(log().contains("realclean"), "a flesh change must realclean: {}", log());

        // Editing a thorn in place — no commit, no thornlist change. This is a
        // first-class workflow, not just a consequence of refetching, and it
        // must rebuild incrementally: `make` decides what the edit costs.
        clear_log();
        let _settled = build(&inst, &machine, "sim", &opts).unwrap();
        clear_log();
        fs::write(repos.join("cactusbase/thorn.cc"), "int a; int b;\n").unwrap();
        let edited = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(edited.rebuilt, "a hand-edited thorn must rebuild");
        assert!(!log().contains("realclean"), "an edit needs no realclean: {}", log());

        // `cactup config delta` reports this same state read-only. Smoke-test
        // it against a real baseline and real divergence.
        crate::commands::delta::config_delta(&inst, Some("sim".into()), true).unwrap();

        // Editing the FLESH in place stays incremental too — a realclean would
        // punish anyone iterating on flesh code.
        clear_log();
        let _settled = build(&inst, &machine, "sim", &opts).unwrap();
        clear_log();
        fs::write(repos.join("core/thorn.cc"), "int a; int c;\n").unwrap();
        let flesh_edit = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(flesh_edit.rebuilt, "an edited flesh must rebuild");
        assert!(
            !log().contains("realclean"),
            "editing the flesh must NOT realclean (only a moved commit does): {}",
            log()
        );

        // No inspectable repo at all (a tree cactup cannot read) ⇒ exactly the
        // pre-source-tracking behavior: no baseline, no rebuild.
        clear_log();
        fs::rename(&repos, cactus.join("repos-away")).unwrap();
        let unknown = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(!unknown.rebuilt, "without inspectable sources nothing is claimed to have changed");
        assert!(log().is_empty(), "no make at all: {}", log());
    }

    #[test]
    fn thornlist_is_a_rebuild_input_and_survives_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, opts) = fake_tree(
            root,
            &format!(
                "sim-config) echo \"$@\" >> {r}/make.log; cd {c}/configs/sim/config-data && \
                 touch cctk_Config.h ;;\n\
                 sim) echo \"$@\" >> {r}/make.log; mkdir -p {c}/exe && \
                 touch {c}/exe/cactus_sim ;;\n\
                 *) echo \"$@\" >> {r}/make.log ;;",
                r = root.display(),
                c = cactus.display()
            ),
        );
        let log = || fs::read_to_string(root.join("make.log")).unwrap_or_default();
        let clear_log = || {
            let _ = fs::remove_file(root.join("make.log"));
        };
        let processed = || fs::read_to_string(cactus.join("configs/sim/cactup-thornlist.th")).unwrap();

        // Build from a custom thornlist (the stock list here is just "A/B").
        let custom = root.join("my.th");
        fs::write(&custom, "A/B\nC/D\n").unwrap();
        let mut custom_opts = BuildOpts::default_for_tests();
        custom_opts.thornlist = Some(custom.clone());
        let first = build(&inst, &machine, "sim", &custom_opts).unwrap();
        assert!(first.rebuilt);
        let canonical = fs::canonicalize(&custom).unwrap().display().to_string();
        assert_eq!(first.meta.thornlist, canonical);
        // The source is snapshotted verbatim, beside the processed copy.
        assert_eq!(
            fs::read_to_string(cactus.join("configs/sim/cactup-thornlist.src.th")).unwrap(),
            "A/B\nC/D\n"
        );

        // A plain rebuild reuses the recorded thornlist rather than reverting to
        // the stock list — and with nothing changed still short-circuits.
        let again = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(!again.rebuilt, "nothing changed, so it must short-circuit");
        assert_eq!(again.meta.thornlist, canonical);
        assert!(processed().contains("C/D"), "must not revert to the stock list: {}", processed());

        // Editing that thornlist in place is picked up with no --thornlist and
        // no -f, and reconfigures without paying for a realclean.
        clear_log();
        fs::write(&custom, "A/B\nC/D\nE/F\n").unwrap();
        let edited = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(edited.rebuilt, "a thornlist edit must rebuild");
        assert!(log().contains("sim-config"), "must reconfigure: {}", log());
        assert!(!log().contains("realclean"), "a thorn change needs no realclean: {}", log());
        assert!(processed().contains("E/F"), "the edit must reach the build: {}", processed());

        // With the source deleted, a forced rebuild falls back to the snapshot
        // instead of silently building the stock list, and still records the
        // original path.
        fs::remove_file(&custom).unwrap();
        let mut forced = BuildOpts::default_for_tests();
        forced.force = true;
        let orphaned = build(&inst, &machine, "sim", &forced).unwrap();
        assert!(orphaned.rebuilt);
        assert_eq!(orphaned.meta.thornlist, canonical);
        assert!(
            processed().contains("E/F"),
            "the snapshot, not the live list, must be what got built: {}",
            processed()
        );

        // Source *and* snapshot gone: refuse to guess, and say what to pass.
        fs::remove_file(cactus.join("configs/sim/cactup-thornlist.src.th")).unwrap();
        let err = match build(&inst, &machine, "sim", &forced) {
            Ok(_) => panic!("should refuse to fall back to the stock thornlist"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("--thornlist"), "should say how to recover: {err}");
    }

    /// The motivating incident, end to end: a thornlist edit that swaps which
    /// arrangement provides a thorn *name* (here, `WeylScal4` moving from
    /// `EinsteinAnalysis` to `SpacetimeX`) must delete that name's stale
    /// per-thorn build state before make runs — but leave an unrelated
    /// thorn's state (`Boundary`, whose provider didn't change) alone.
    /// Real repos are not needed: without them source tracking simply reads
    /// as Unknown, while `thorn_providers()` still parses fine off the
    /// thornlist text alone.
    #[test]
    fn provider_swap_invalidates_per_thorn_build_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, opts) = fake_tree(
            root,
            &format!(
                "sim-config) echo \"$@\" >> {r}/make.log; cd {c}/configs/sim/config-data && \
                 touch cctk_Config.h ;;\n\
                 sim) echo \"$@\" >> {r}/make.log; mkdir -p {c}/exe && \
                 touch {c}/exe/cactus_sim ;;\n\
                 *) echo \"$@\" >> {r}/make.log ;;",
                r = root.display(),
                c = cactus.display()
            ),
        );
        let log = || fs::read_to_string(root.join("make.log")).unwrap_or_default();
        let clear_log = || {
            let _ = fs::remove_file(root.join("make.log"));
        };

        let list = root.join("crl.th");
        fs::write(
            &list,
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://e.invalid/einsteinanalysis.git\n\
             !CHECKOUT = EinsteinAnalysis/WeylScal4 CactusBase/Boundary\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://e.invalid/spacetimex.git\n\
             !CHECKOUT = SpacetimeX/Dummy\n\
             #DISABLED SpacetimeX/WeylScal4\n",
        )
        .unwrap();

        let mut first_opts = BuildOpts::default_for_tests();
        first_opts.thornlist = Some(list.clone());
        let first = build(&inst, &machine, "sim", &first_opts).unwrap();
        assert!(first.rebuilt);
        let providers = first.meta.thorn_providers.clone().expect("providers must be recorded");
        assert_eq!(providers["WeylScal4"], "arrangements/EinsteinAnalysis/WeylScal4");
        assert!(providers.contains_key("Boundary"));

        // Fabricate stale per-thorn build state, as a real prior build would
        // have left behind: a build/<Thorn>/ directory with a stale .d file,
        // and an archive `ar` would otherwise update in place.
        let config_dir = cactus.join("configs/sim");
        fs::create_dir_all(config_dir.join("build/WeylScal4")).unwrap();
        fs::write(config_dir.join("build/WeylScal4/Kranc.cc.d"), "stale\n").unwrap();
        fs::create_dir_all(config_dir.join("build/Boundary")).unwrap();
        fs::write(config_dir.join("build/Boundary/some.o"), "stale\n").unwrap();
        fs::write(config_dir.join("lib/libthorn_WeylScal4.a"), "stale\n").unwrap();
        fs::write(config_dir.join("lib/libthorn_Boundary.a"), "stale\n").unwrap();

        // Overwrite the old list file in place with the swap, and rebuild
        // with no --thornlist — the stored path is picked up, exactly the
        // "edit the thornlist" workflow.
        fs::write(
            &list,
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://e.invalid/einsteinanalysis.git\n\
             !CHECKOUT = CactusBase/Boundary\n\
             #DISABLED EinsteinAnalysis/WeylScal4\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://e.invalid/spacetimex.git\n\
             !CHECKOUT = SpacetimeX/Dummy SpacetimeX/WeylScal4\n",
        )
        .unwrap();
        clear_log();
        let swapped = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(swapped.rebuilt, "a provider swap must rebuild");
        assert!(log().contains("sim-config"), "must reconfigure: {}", log());
        assert!(!log().contains("realclean"), "a provider swap needs no realclean: {}", log());

        assert!(
            !config_dir.join("build/WeylScal4").exists(),
            "the old provider's build state must be gone"
        );
        assert!(
            !config_dir.join("lib/libthorn_WeylScal4.a").exists(),
            "the old provider's archive must be gone"
        );
        assert!(
            config_dir.join("build/Boundary").is_dir(),
            "an unrelated thorn's build state must survive untouched"
        );
        assert!(
            config_dir.join("lib/libthorn_Boundary.a").is_file(),
            "an unrelated thorn's archive must survive untouched"
        );

        let new_providers = swapped.meta.thorn_providers.clone().expect("providers must be recorded");
        assert_eq!(new_providers["WeylScal4"], "arrangements/SpacetimeX/WeylScal4");
    }

    /// The up-to-date short-circuit must also adopt a missing
    /// `thorn_providers` baseline (mirroring what it already does for
    /// `sources`), or a config built before provenance tracking landed could
    /// never acquire one: every future build would short-circuit here and a
    /// later provider swap would go unnoticed.
    #[test]
    fn build_adopts_missing_thorn_providers_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, opts) = fake_tree(
            root,
            &format!(
                "sim-config) cd {c}/configs/sim/config-data && touch cctk_Config.h ;;\n\
                 sim) mkdir -p {c}/exe && touch {c}/exe/cactus_sim ;;",
                c = cactus.display()
            ),
        );

        let list = root.join("crl.th");
        fs::write(
            &list,
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://e.invalid/repo.git\n\
             !CHECKOUT = CactusBase/Boundary\n",
        )
        .unwrap();
        let mut first_opts = BuildOpts::default_for_tests();
        first_opts.thornlist = Some(list.clone());
        let first = build(&inst, &machine, "sim", &first_opts).unwrap();
        assert!(first.rebuilt);
        assert!(first.meta.thorn_providers.is_some());

        // Simulate a config built before provenance tracking landed: the
        // field is simply absent (no schema bump, no migration).
        let mut stale = ConfigMeta::load(&cactus, "sim").unwrap().unwrap();
        stale.thorn_providers = None;
        stale.store(&cactus).unwrap();

        let again = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(!again.rebuilt, "nothing changed besides the missing baseline");
        let reloaded = ConfigMeta::load(&cactus, "sim").unwrap().unwrap();
        assert!(reloaded.thorn_providers.is_some(), "the baseline must be adopted on disk");
    }

    /// A single-thorn CRL list plus a real thorn directory on disk, shared by
    /// the `thorn_shapes` build-level tests below. The thorn dir is a plain
    /// directory (not a symlink into `repos/`), so item 2 of the fingerprint
    /// is the `<dir>` marker and item 3 (repo URL) is omitted — exactly the
    /// "hand-placed arrangement" / test-fixture case, and it needs no real
    /// git repo to exercise the file-content half of the mechanism.
    fn write_shape_fixture(cactus: &Path) -> (PathBuf, PathBuf) {
        let list = cactus.join("crl.th");
        fs::write(
            &list,
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://e.invalid/testarr.git\n\
             !CHECKOUT = TestArr/TestThorn\n",
        )
        .unwrap();
        let thorn_dir = cactus.join("arrangements/TestArr/TestThorn");
        fs::create_dir_all(thorn_dir.join("src")).unwrap();
        fs::write(thorn_dir.join("configuration.ccl"), "REQUIRES GenericFD\n").unwrap();
        fs::write(thorn_dir.join("src/make.code.defn"), "SRCS = thorn.cc\n").unwrap();
        fs::write(thorn_dir.join("src/thorn.cc"), "int a;\n").unwrap();
        (list, thorn_dir)
    }

    /// Fabricate stale per-thorn build state exactly as a real prior build
    /// would leave behind — same shape as `provider_swap_invalidates_per_thorn_build_state`.
    fn fabricate_stale_thorn_state(config_dir: &Path, thorn: &str) {
        fs::create_dir_all(config_dir.join("build").join(thorn)).unwrap();
        fs::write(config_dir.join("build").join(thorn).join("Kranc.cc.d"), "stale\n").unwrap();
        fs::write(config_dir.join("lib").join(format!("libthorn_{thorn}.a")), "stale\n").unwrap();
    }

    fn stale_thorn_state_gone(config_dir: &Path, thorn: &str) -> bool {
        !config_dir.join("build").join(thorn).exists()
            && !config_dir.join("lib").join(format!("libthorn_{thorn}.a")).exists()
    }

    /// The real incident this whole mechanism exists for: a thorn's
    /// `configuration.ccl` `REQUIRES` goes non-empty -> empty (here, deleted
    /// entirely) with the thornlist itself byte-identical. `provider_delta`
    /// is blind to this — the provider never moved — so without
    /// `thorn_shapes` the stale `build/<Thorn>/` would survive into a
    /// reconfigure that just deleted the bindings header it references.
    #[test]
    fn ccl_change_invalidates_per_thorn_build_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, opts) = fake_tree(
            root,
            &format!(
                "sim-config) echo \"$@\" >> {r}/make.log; cd {c}/configs/sim/config-data && \
                 touch cctk_Config.h ;;\n\
                 sim) echo \"$@\" >> {r}/make.log; mkdir -p {c}/exe && \
                 touch {c}/exe/cactus_sim ;;\n\
                 *) echo \"$@\" >> {r}/make.log ;;",
                r = root.display(),
                c = cactus.display()
            ),
        );
        let log = || fs::read_to_string(root.join("make.log")).unwrap_or_default();
        let clear_log = || {
            let _ = fs::remove_file(root.join("make.log"));
        };

        let (list, thorn_dir) = write_shape_fixture(&cactus);
        let mut first_opts = BuildOpts::default_for_tests();
        first_opts.thornlist = Some(list);
        let first = build(&inst, &machine, "sim", &first_opts).unwrap();
        assert!(first.rebuilt);
        assert!(first.meta.thorn_shapes.as_ref().unwrap().contains_key("TestThorn"));

        let config_dir = cactus.join("configs/sim");
        fabricate_stale_thorn_state(&config_dir, "TestThorn");

        // Rewrite configuration.ccl to a comment-only file: same thornlist,
        // same provider, different shape.
        fs::write(thorn_dir.join("configuration.ccl"), "# no longer requires anything\n").unwrap();
        clear_log();
        let edited = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(edited.rebuilt, "a shape change must rebuild");
        assert!(log().contains("sim-config"), "must reconfigure: {}", log());
        assert!(!log().contains("realclean"), "a shape change needs no realclean: {}", log());
        assert!(
            stale_thorn_state_gone(&config_dir, "TestThorn"),
            "the stale per-thorn build state must be removed"
        );
    }

    /// The second incident: a source file removed from a thorn's `src/`.
    /// Cactus's `ar` would otherwise update `libthorn_<Thorn>.a` in place,
    /// letting the orphan `.o` link in silently — so the whole per-thorn
    /// build state must be invalidated instead.
    #[test]
    fn removed_source_file_invalidates_per_thorn_build_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, opts) = fake_tree(
            root,
            &format!(
                "sim-config) echo \"$@\" >> {r}/make.log; cd {c}/configs/sim/config-data && \
                 touch cctk_Config.h ;;\n\
                 sim) echo \"$@\" >> {r}/make.log; mkdir -p {c}/exe && \
                 touch {c}/exe/cactus_sim ;;\n\
                 *) echo \"$@\" >> {r}/make.log ;;",
                r = root.display(),
                c = cactus.display()
            ),
        );
        let log = || fs::read_to_string(root.join("make.log")).unwrap_or_default();
        let clear_log = || {
            let _ = fs::remove_file(root.join("make.log"));
        };

        let (list, thorn_dir) = write_shape_fixture(&cactus);
        // A second source file that will be removed between builds.
        fs::write(thorn_dir.join("src/extra.cc"), "int b;\n").unwrap();
        let mut first_opts = BuildOpts::default_for_tests();
        first_opts.thornlist = Some(list);
        let first = build(&inst, &machine, "sim", &first_opts).unwrap();
        assert!(first.rebuilt);

        let config_dir = cactus.join("configs/sim");
        fabricate_stale_thorn_state(&config_dir, "TestThorn");

        fs::remove_file(thorn_dir.join("src/extra.cc")).unwrap();
        clear_log();
        let edited = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(edited.rebuilt, "a removed source file must rebuild");
        assert!(!log().contains("realclean"), "needs no realclean: {}", log());
        assert!(
            stale_thorn_state_gone(&config_dir, "TestThorn"),
            "the stale per-thorn build state must be removed"
        );
    }

    /// The critical negative case: editing only the *body* of an ordinary
    /// source file — no file added or removed, no `.ccl`/`make.*` touched —
    /// must NOT be treated as a shape change. `make`'s own `.d` dependency
    /// tracking is what should handle this, exactly as it does for the flesh
    /// (`SourceDelta::Edited`); if `thorn_shapes` hashed body content, every
    /// edit would wipe a thorn's whole build directory, punishing anyone
    /// iterating on sources.
    #[test]
    fn body_edit_does_not_wipe_per_thorn_build_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, opts) = fake_tree(
            root,
            &format!(
                "sim-config) echo \"$@\" >> {r}/make.log; cd {c}/configs/sim/config-data && \
                 touch cctk_Config.h ;;\n\
                 sim) echo \"$@\" >> {r}/make.log; mkdir -p {c}/exe && \
                 touch {c}/exe/cactus_sim ;;\n\
                 *) echo \"$@\" >> {r}/make.log ;;",
                r = root.display(),
                c = cactus.display()
            ),
        );
        let clear_log = || {
            let _ = fs::remove_file(root.join("make.log"));
        };

        let (list, thorn_dir) = write_shape_fixture(&cactus);
        let mut first_opts = BuildOpts::default_for_tests();
        first_opts.thornlist = Some(list);
        let first = build(&inst, &machine, "sim", &first_opts).unwrap();
        assert!(first.rebuilt);
        let first_shape = first.meta.thorn_shapes.as_ref().unwrap()["TestThorn"].clone();

        let config_dir = cactus.join("configs/sim");
        fabricate_stale_thorn_state(&config_dir, "TestThorn");

        // Edit the body of the ordinary source file only.
        fs::write(thorn_dir.join("src/thorn.cc"), "int a; int b;\n").unwrap();
        clear_log();
        let edited = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(!edited.rebuilt, "a body-only edit must not by itself trigger a rebuild");
        assert_eq!(
            edited.meta.thorn_shapes.as_ref().unwrap()["TestThorn"], first_shape,
            "the fingerprint must be blind to ordinary source content"
        );
        assert!(
            !stale_thorn_state_gone(&config_dir, "TestThorn"),
            "make must be left to handle a body edit — the per-thorn build state must survive"
        );

        // The assertion above is necessary but not sufficient: nothing was
        // rebuilt, so the deletion block never ran and could not have wiped
        // anything regardless. Force a real `Incremental` pass — a *second*
        // thorn changing shape in the same build — and check that the
        // invalidation is scoped to that thorn and does not sweep up the
        // body-edited one alongside it.
        let other_dir = cactus.join("arrangements/TestArr/OtherThorn");
        fs::create_dir_all(other_dir.join("src")).unwrap();
        fs::write(other_dir.join("configuration.ccl"), "REQUIRES GenericFD\n").unwrap();
        fs::write(other_dir.join("src/make.code.defn"), "SRCS = other.cc\n").unwrap();
        fs::write(other_dir.join("src/other.cc"), "int c;\n").unwrap();
        let list = cactus.join("crl.th");
        fs::write(
            &list,
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://e.invalid/testarr.git\n\
             !CHECKOUT = TestArr/TestThorn TestArr/OtherThorn\n",
        )
        .unwrap();
        clear_log();
        build(&inst, &machine, "sim", &opts).unwrap();

        // Both thorns now have a recorded baseline and fabricated stale state.
        fabricate_stale_thorn_state(&config_dir, "TestThorn");
        fabricate_stale_thorn_state(&config_dir, "OtherThorn");

        // One body edit, one shape change, in the same build.
        fs::write(thorn_dir.join("src/thorn.cc"), "int a; int b; int c;\n").unwrap();
        fs::write(other_dir.join("configuration.ccl"), "# nothing required now\n").unwrap();
        clear_log();
        let mixed = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(mixed.rebuilt, "a shape change must trigger a rebuild");
        assert!(
            stale_thorn_state_gone(&config_dir, "OtherThorn"),
            "the reshaped thorn's stale build state must be removed"
        );
        assert!(
            !stale_thorn_state_gone(&config_dir, "TestThorn"),
            "invalidation must be scoped per thorn: a body-edited thorn keeps its build state \
             even while another thorn is being invalidated in the same build"
        );
    }

    /// Defect A, configure-failure branch: the marker is absent (configure
    /// never completed). The message still names `cctk_Config.h`.
    #[test]
    fn incomplete_without_configure_names_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Neither step produces anything; both "succeed".
        let (_mdb, machine, inst, opts) =
            fake_tree(root, "sim-config) exit 0 ;;\nsim) exit 0 ;;");

        let err = match build(&inst, &machine, "sim", &opts) {
            Ok(_) => panic!("build should have failed as incomplete"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("cctk_Config.h"), "should name the configure marker: {err}");
    }
}
