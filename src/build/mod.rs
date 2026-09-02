//! The build engine behind `cactup config build` (spec §7):
//! optionlist selection + render + flag injection (§7.8), thornlist toggles
//! (§7.5, D8), env-setup'd `make` driving (§7.2, §6.1), build universes
//! (§4.8), the rebuild-decision snapshot diff (§7.8), the per-config build
//! lock (§2.3 item 4), and `cactup-config.toml` metadata (§7.4).

pub mod attempt;

use crate::args::{BuildOpts, MakeJobs, TopologyFlags};
use crate::build::attempt::{BuildAttempt, BuildMeta, BuildOutcomeRecord, Reservation, Timestamps};
use crate::database::SCHEMA;
use crate::fetch::SourceHeads;
use crate::installation::Installation;
use crate::lock::LinkLock;
use crate::mdb::meta::Phase;
use crate::mdb::{Machine, Optionlist};
use crate::sim::restart::{freeze_vars, thaw_vars, UniverseSpec, NO_JOB_ID};
use crate::sim::start::{script_command, spawn_and_wait, write_executable};
use crate::sim::vars::QueueFit;
use crate::template::{VarSet, VarValue};
use crate::thornlist::Thornlist;
use crate::Res;
use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// Which optionlist this config is built from, and hence rebuilt from
    /// (§7.8). Flattened, so on disk this is the single key it names.
    #[serde(flatten)]
    pub optionlist_source: OptionlistSource,
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

/// Which optionlist a config is built from (§4.4, §7.8): one of the machine's
/// own variants, or a file the user pointed `--optionlist` at. Never both —
/// the two flags conflict at the cli, so a config is on record as one or the
/// other, and re-supplying either flag is what moves it between them.
///
/// Serialized flattened into `cactup-config.toml`, where it is the single key
/// `variant = "cuda"` or `optionlist = "/abs/path/my.cfg"`. Modelling it as a
/// sum rather than two optional keys is what keeps "exactly one" true by
/// construction: neither the metadata nor the resolver below can express a
/// config that is both, or neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OptionlistSource {
    /// An optionlist variant named by the machine definition (§4.4).
    Variant(String),
    /// A canonicalized path, as given to `--optionlist`. Canonical because a
    /// relative path would resolve against whatever directory a later rebuild
    /// happened to run from.
    Optionlist(String),
}

/// What every display site prints for "which optionlist": the variant name, or
/// the path. Unambiguous in practice because the path is absolute, and a bare
/// variant name never is.
impl std::fmt::Display for OptionlistSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OptionlistSource::Variant(v) => f.write_str(v),
            OptionlistSource::Optionlist(p) => f.write_str(p),
        }
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
/// The optionlist source snapshot: the verbatim text the config was built
/// from, which the §7.8 rebuild decision diffs and which stands in for a
/// `--optionlist` file that has since moved or been deleted. Named `.toml`
/// from when an optionlist could only be mdb TOML; it now holds whichever of
/// the three accepted forms the source was written in.
pub const OPTIONLIST_SNAPSHOT: &str = "cactup-optionlist.toml";

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
        if let Some(renamed) = renamed_legacy_thornlist(Path::new(stored_path))
            && let Ok(text) = fs::read_to_string(&renamed)
        {
            return Ok(ResolvedThornlist {
                recorded: renamed.display().to_string(),
                text,
                from_snapshot: false,
            });
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

/// Whether an `enabled-thorns`/`disabled-thorns` entry names `thorn`: an
/// entry matches a thorn line's full `arrangement/Thorn`, or its bare thorn
/// name after the `/`.
fn thorn_spec_matches(spec: &str, thorn: &str) -> bool {
    thorn == spec || thorn.rsplit('/').next() == Some(spec)
}

/// Apply the §7.5 (D8) machine thorn toggles to a thornlist's contents:
/// `disabled-thorns` entries get a `#DISABLED ` prefix, `enabled-thorns`
/// entries get it removed. Entries match a thorn line's `arrangement/Thorn`
/// (or bare thorn name after `/`).
pub fn apply_thorn_toggles(thornlist: &str, enabled: &[String], disabled: &[String]) -> String {
    thornlist
        .lines()
        .map(|line| {
            let bare = line.strip_prefix("#DISABLED ").unwrap_or(line);
            let thorn = bare.trim();
            if thorn.is_empty() || thorn.starts_with('#') || thorn.starts_with('!') {
                return line.to_owned();
            }
            if disabled.iter().any(|d| thorn_spec_matches(d, thorn)) {
                format!("#DISABLED {bare}")
            } else if line.starts_with("#DISABLED ")
                && enabled.iter().any(|e| thorn_spec_matches(e, thorn))
            {
                bare.to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Which source of a `disabled-thorns` entry (§7.5, §7.8) switched a thorn
/// off — the two layers `prepare` merges, kept apart only so the warning can
/// name the file to go edit.
#[derive(Debug, PartialEq, Eq)]
pub enum DisabledBy {
    /// The machine's own `[build].disabled-thorns` (§7.5).
    Machine,
    /// The selected optionlist variant's `[cactup].disabled-thorns` (§7.8).
    Optionlist,
}

/// One thorn the source thornlist actively enables that a `disabled-thorns`
/// entry is about to switch back off.
#[derive(Debug, PartialEq, Eq)]
pub struct DisabledOverride {
    /// The thorn as the thornlist spells it (`arrangement/Thorn`).
    pub thorn: String,
    /// The `disabled-thorns` entry that matched — not always equal to
    /// `thorn`, since an entry may be a bare thorn name.
    pub spec: String,
    pub by: DisabledBy,
}

/// Thorns the source thornlist *explicitly enables* that the mdb then
/// disables (§7.5 machine-level, §7.8 optionlist-level), in thornlist order.
///
/// `apply_thorn_toggles` performs this override silently, and a thorn the
/// user deliberately put in their thornlist quietly missing from the built
/// config is precisely the kind of thing that gets debugged for an hour —
/// so `prepare` warns loudly about every one. A thorn the list already
/// carries as `#DISABLED` is not a conflict: the list and the toggle agree.
pub fn disabled_thorn_overrides(
    thornlist: &str,
    machine_disabled: &[String],
    optionlist_disabled: &[String],
) -> Vec<DisabledOverride> {
    thornlist
        .lines()
        .filter_map(|line| {
            // Same notion of "a thorn line" as apply_thorn_toggles, with
            // `#DISABLED ` deliberately NOT stripped: a `#`-prefixed line is
            // either prose or a thorn the list itself already turned off, and
            // neither is the thornlist enabling anything.
            let thorn = line.trim();
            if thorn.is_empty() || thorn.starts_with('#') || thorn.starts_with('!') {
                return None;
            }
            // Machine first: the variant layers on top of the machine list, so
            // a thorn both of them disable is attributed to the machine.
            let (spec, by) = machine_disabled
                .iter()
                .find(|d| thorn_spec_matches(d, thorn))
                .map(|d| (d, DisabledBy::Machine))
                .or_else(|| {
                    optionlist_disabled
                        .iter()
                        .find(|d| thorn_spec_matches(d, thorn))
                        .map(|d| (d, DisabledBy::Optionlist))
                })?;
            Some(DisabledOverride { thorn: thorn.to_owned(), spec: spec.clone(), by })
        })
        .collect()
}

/// Print the loud yellow warning for every thorn
/// [`disabled_thorn_overrides`] found, naming the `disabled-thorns` entry
/// and the layer it came from so the user knows which file to edit.
/// Silent when there is no conflict.
fn warn_disabled_overrides(
    thornlist: &str,
    machine_disabled: &[String],
    optionlist_disabled: &[String],
    machine_name: &str,
    optionlist_source: &OptionlistSource,
) {
    let overrides = disabled_thorn_overrides(thornlist, machine_disabled, optionlist_disabled);
    if overrides.is_empty() {
        return;
    }
    println!(
        "{}",
        format!(
            "warning: the thornlist enables {} thorn(s) that the mdb disables — they will NOT \
             be built:",
            overrides.len(),
        )
        .yellow()
        .bold()
    );
    for DisabledOverride { thorn, spec, by } in &overrides {
        // §7.5 vs §7.8 — the two layers live in different files.
        let source = match by {
            DisabledBy::Machine => format!("machine {machine_name}"),
            DisabledBy::Optionlist => format!("optionlist {optionlist_source}"),
        };
        println!(
            "{}",
            format!("  {thorn} — disabled-thorns = [\"{spec}\"] ({source})").yellow()
        );
    }
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

/// The `Full` reason for a flesh-level source change, named because `build()`
/// keys an extra explanatory line off exactly this decision. Matching the
/// literal in both places would break silently the moment either is reworded.
///
/// Deliberately not "moved to a different commit": the flesh also reaches this
/// state by ceasing to be readable as a git repo at all (`SourceChange`'s
/// `vanished`), and a reason line that named the wrong cause would be worse
/// than a general one.
const FLESH_NOT_AS_BUILT: &str = "the Cactus flesh is not the commit this was built from";

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
    /// Repos the build recorded a state for that can no longer be read: the
    /// directory is gone, or it is no longer a git repo (a hand-built variant
    /// dropped in place of the checkout). No state string exists to compare,
    /// which is precisely why this is a change and not a skip.
    pub vanished: Vec<String>,
}

impl SourceChange {
    fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.edited.is_empty() && self.vanished.is_empty()
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
///
/// The opposite direction is NOT tolerated: a repo the stored record knows
/// about that the live tree can no longer produce a state string for — gone
/// from disk, or no longer a git repo because someone swapped in a hand-built
/// variant — is `vanished`, and counts exactly like a moved commit. Walking
/// only `fresh.heads` used to make that case invisible, so replacing a
/// checkout with a non-git copy of it reported "matches what this config was
/// built from".
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
    for repo in fresh.unreadable.iter().chain(fresh.missing.iter()) {
        if stored.contains_key(repo.as_str()) {
            change.vanished.push(repo.clone());
        }
    }
    change.vanished.sort();
    if change.is_empty() {
        return (SourceDelta::Unchanged, change);
    }
    // A vanished repo is treated like a moved one, flesh included: whatever is
    // there now is provably not the commit that was compiled, and a flesh that
    // can no longer be identified invalidates every object just as a flesh
    // that moved does.
    let flesh_gone = fresh
        .flesh
        .as_deref()
        .is_some_and(|f| change.moved.iter().chain(change.vanished.iter()).any(|m| m == f));
    let delta = if flesh_gone {
        SourceDelta::Flesh
    } else if !change.moved.is_empty() || !change.vanished.is_empty() {
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
/// every thorn it provides. Takes the cache behind a `Mutex`, not `&mut`:
/// `thorn_shapes` below fans this out across `par::parallel_map`'s worker
/// pool, so several threads can look a repo's URL up concurrently.
fn shape_repo_url(repo_dir: &Path, cache: &Mutex<HashMap<PathBuf, Option<String>>>) -> Option<String> {
    if let Some(cached) = cache.lock().expect("thorn_shapes url cache poisoned").get(repo_dir) {
        return cached.clone();
    }
    let url = (|| {
        let repo = gix::open(repo_dir).ok()?;
        let remote = repo.find_remote("origin").ok()?;
        let raw = remote.url(gix::remote::Direction::Fetch)?.to_bstring().to_string();
        Some(crate::fetch::git::normalize_url(&raw))
    })();
    cache.lock().expect("thorn_shapes url cache poisoned").insert(repo_dir.to_owned(), url.clone());
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
/// ran — produces no spurious delta. A single thorn's read failure therefore
/// never surfaces as an `Err` from `thorn_shapes` — only an interrupt does
/// (see below): a build must never fail merely because a thorn directory
/// happened to be unreadable.
fn shape_one_thorn(
    cactus_root: &Path,
    repos_root: Option<&Path>,
    url_cache: &Mutex<HashMap<PathBuf, Option<String>>>,
    provider: &str,
) -> Option<String> {
    let thorn_dir = cactus_root.join(provider);

    let files = shape_files(&thorn_dir).ok()?;
    let link_meta = fs::symlink_metadata(&thorn_dir).ok()?;
    let shape_marker: Vec<u8> = if link_meta.file_type().is_symlink() {
        fs::read_link(&thorn_dir).ok()?.to_string_lossy().into_owned().into_bytes()
    } else {
        b"<dir>".to_vec()
    };

    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    feed(&mut hasher, provider.as_bytes());
    feed(&mut hasher, &shape_marker);

    if let Some(repos_root) = repos_root
        && let Ok(real) = fs::canonicalize(&thorn_dir)
        && let Some(repo_dir) = shape_repo_dir(&real, repos_root)
        && let Some(url) = shape_repo_url(&repo_dir, url_cache)
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
        let bytes = fs::read(thorn_dir.join(f)).ok()?;
        feed(&mut hasher, f.as_bytes());
        feed(&mut hasher, &bytes);
    }

    let id = hasher.try_finalize().ok()?;
    Some(id.to_hex_with_len(16).to_string())
}

/// Per-thorn "shape" fingerprints, fanned out across [`crate::par::parallel_map`]
/// (§2.4's parallelism contract: each thorn's probe is its own handful of
/// stats/reads, latency-bound on network filesystems, and a real tree carries
/// ~400 of them — sequentially that is easily multi-second). `progress` is
/// init'ed to the thorn count and driven exactly like
/// [`crate::fetch::source_heads`]'s repo loop: a short-lived child per
/// in-flight thorn, `inc()` per completion (§2.4's progress contract — this
/// phase never calls `info`/`fail` on an item, only `init`/`add_child`/`inc`).
///
/// Returns `Err` only when interrupted (`par::parallel_map`'s "interrupted"
/// failure, §2.4's interrupt contract) — see `shape_one_thorn`'s doc comment
/// for why an individual unreadable thorn never causes one.
pub fn thorn_shapes(
    cactus_root: &Path,
    list: &Thornlist,
    progress: &mut prodash::tree::Item,
) -> Res<BTreeMap<String, String>> {
    let repos_root = fs::canonicalize(cactus_root.join("repos")).ok();
    let url_cache: Mutex<HashMap<PathBuf, Option<String>>> = Mutex::new(HashMap::new());
    let providers: Vec<(String, String)> = list.thorn_providers().into_iter().collect();

    progress.init(Some(providers.len()), Some(prodash::unit::label("thorns")));
    let progress = Mutex::new(progress);

    let results = crate::par::parallel_map(&providers, |(name, provider)| {
        let current = progress.lock().expect("thorn_shapes progress poisoned").add_child(name.clone());
        let shape = shape_one_thorn(cactus_root, repos_root.as_deref(), &url_cache, provider);
        drop(current);
        progress.lock().expect("thorn_shapes progress poisoned").inc();
        shape.map(|hash| (name.clone(), hash))
    })?;

    Ok(results.into_iter().flatten().collect())
}

/// [`thorn_shapes`] for a call site with no progress tree of its own to hang
/// a child off — mirrors [`crate::fetch::source_heads_with_progress`]. Uses
/// `setup_prodash_if_tty`: correct here specifically because `thorn_shapes`
/// never calls `info`/`fail` on its progress item (see
/// `manifest::setup_prodash_if_tty`'s doc comment on why that precondition
/// matters — a phase that DID call them would lose those messages on a
/// non-tty stderr).
pub fn thorn_shapes_with_progress(cactus_root: &Path, list: &Thornlist) -> Res<BTreeMap<String, String>> {
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let mut probing = progress.add_child("probe thorn shapes");
    let result = thorn_shapes(cactus_root, list, &mut probing);
    drop(probing);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    result
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
        Some(_) if sources == SourceDelta::Flesh => RebuildDecision::Full(FLESH_NOT_AS_BUILT),
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
            RebuildDecision::Incremental("thorn sources are not the commits this was built from")
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

/// The optionlist a build will use, and where it came from (§4.4, §7.8).
pub struct ResolvedOptionlist {
    /// Recorded verbatim in `cactup-config.toml`, and what makes the choice
    /// sticky across later bare rebuilds.
    pub source: OptionlistSource,
    pub optionlist: Optionlist,
    /// Set when the `--optionlist` file this config records was unreadable and
    /// its snapshot stood in for it.
    pub from_snapshot: bool,
}

/// Resolve the optionlist for a build (§4.4, §7.8):
///
///   1. `--optionlist PATH` — explicit; a hard error if unreadable.
///   2. `--variant NAME` — explicit.
///   3. whichever of the two this config was last built from, since it records
///      exactly one:
///      - a variant, when the machine still lists it; a hard error naming what
///        went missing when it does not.
///      - the `--optionlist` file, falling back to the config's verbatim
///        snapshot when that path has since moved or been deleted.
///   4. the machine's variant selection — the sole variant, or the one marked
///      `default = true` (§4.4).
///
/// Steps 1 and 2 are alternatives, not a precedence chain, and so are the two
/// halves of step 3: `--optionlist` and `--variant` conflict at the cli, and
/// supplying either **replaces** what the config was on record as being. That
/// is the whole rule — whichever flag was passed most recently is what sticks,
/// and neither one has to be repeated to keep sticking.
///
/// Step 3 mirrors `resolve_thornlist` and exists for the same reason: a
/// rebuild must not silently build something other than what the config is on
/// record as being. All three of the "what is this config made of" flags
/// behave the same way here — `--thornlist` (§7.5), `--optionlist` and
/// `--variant` — so there is no rule to remember about which are remembered.
///
/// Preferring the live file over the snapshot is deliberate: editing the
/// optionlist in place and rebuilding is the whole point of naming one, and
/// that edit must be picked up (as a full rebuild, since the §7.8 source diff
/// sees it). A recorded *variant* has no snapshot equivalent because a
/// snapshot records the text one build used, not a standing definition of a
/// variant the mdb has since dropped.
pub fn resolve_optionlist(
    cactus_root: &Path,
    name: &str,
    machine: &Machine,
    stored: Option<&ConfigMeta>,
    opts: &BuildOpts,
) -> Res<ResolvedOptionlist> {
    if let Some(path) = &opts.optionlist {
        let optionlist = Optionlist::load_any(path)?;
        // Canonicalize what we record: a relative path would resolve against
        // whatever directory a later rebuild happened to run from.
        let recorded = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        return Ok(ResolvedOptionlist {
            source: OptionlistSource::Optionlist(recorded.display().to_string()),
            optionlist,
            from_snapshot: false,
        });
    }

    // Everything below is the sticky path, so an explicit `--variant` skips
    // all of it — that flag IS how a config is deliberately moved onto the
    // machine's own variants, whether from another variant or from a
    // `--optionlist` file. It displaces, exactly as `--optionlist` above did.
    if opts.variant.is_none()
        && let Some(stored) = stored
    {
        match &stored.optionlist_source {
            OptionlistSource::Variant(recorded) => {
                let listed = &machine.meta.variants.optionlist.variants;
                if listed.contains(recorded) {
                    let optionlist = Optionlist::load(&machine.optionlist_path(recorded))?;
                    return Ok(ResolvedOptionlist {
                        source: stored.optionlist_source.clone(),
                        optionlist,
                        from_snapshot: false,
                    });
                }
                // Renamed or dropped from the mdb since this config was built.
                // Unlike a vanished `--optionlist` file there is nothing to
                // fall back on — the config's snapshot is the *rendered*
                // inputs of one particular variant, not a standing definition
                // of it — so say plainly what went missing rather than
                // silently resolving to whatever the machine now calls its
                // default.
                bail!(
                    "config \"{name}\" was built with optionlist variant \"{recorded}\", which \
                     machine \"{}\" no longer has (it now offers: {}) — pass --variant to pick \
                     one of those, or --optionlist to build from a file",
                    machine.name,
                    if listed.is_empty() { "none".to_owned() } else { listed.join(", ") },
                );
            }
            OptionlistSource::Optionlist(recorded) => {
                if let Ok(optionlist) = Optionlist::load_any(Path::new(recorded)) {
                    return Ok(ResolvedOptionlist {
                        source: stored.optionlist_source.clone(),
                        optionlist,
                        from_snapshot: false,
                    });
                }
                let snapshot = config_file(cactus_root, name, OPTIONLIST_SNAPSHOT);
                let source = fs::read_to_string(&snapshot).with_context(|| {
                    format!(
                        "config \"{name}\" was built from optionlist {recorded}, which is no \
                         longer readable, and there is no snapshot at {} to fall back on — pass \
                         --optionlist to say which optionlist to build from, or --variant to \
                         switch to one of the machine's own",
                        snapshot.display()
                    )
                })?;
                let optionlist = Optionlist::parse_any(&source).with_context(|| {
                    format!("invalid optionlist snapshot {}", snapshot.display())
                })?;
                return Ok(ResolvedOptionlist {
                    source: stored.optionlist_source.clone(),
                    optionlist,
                    from_snapshot: true,
                });
            }
        }
    }

    let variant = machine.select_optionlist(opts.variant.as_deref())?;
    let optionlist = Optionlist::load(&machine.optionlist_path(&variant))?;
    Ok(ResolvedOptionlist {
        source: OptionlistSource::Variant(variant),
        optionlist,
        from_snapshot: false,
    })
}

/// The §4.4/D12 queue-compatibility facts, given an already-resolved
/// optionlist. Split out so `prepare` can reuse the resolution it has already
/// done rather than reading the optionlist a second time, while `queue_fit`
/// stays the one entry point for callers that have nothing resolved yet.
fn fit_from(machine: &Machine, name: &str, opts: &BuildOpts, optionlist: &Optionlist) -> QueueFit {
    let universe = resolve_build_universe(
        opts,
        optionlist.header.universe.as_deref(),
        machine.meta.build.universe.as_deref(),
        machine.meta.declared_host().is_some(),
    )
    .map(str::to_owned);
    QueueFit {
        universe,
        compatible_queues: optionlist.header.compatible_queues.clone(),
        gpu: optionlist.header.gpu,
        label: name.to_owned(),
    }
}

/// The §4.4/D12 queue-compatibility facts for `name` on `machine`: whatever
/// `resolve_optionlist` picks (the MDB variant, or the sticky `--optionlist`
/// file this config already records) fed through `resolve_build_universe`.
/// `prepare` needs these as part of composing a
/// config, and `build submit` needs them BEFORE `prepare` runs — a topology
/// (and the reservation `prepare` must be handed) can't be resolved without
/// knowing which queues this build is compatible with, but that compatibility
/// is itself an optionlist-header fact `prepare` alone used to derive. Both
/// paths route through this one function so they can never silently disagree
/// about which queues a build may land on.
pub fn queue_fit(cactus_root: &Path, machine: &Machine, name: &str, opts: &BuildOpts) -> Res<QueueFit> {
    // Loaded here rather than taken as an argument because the stored
    // metadata is what makes a `--optionlist` choice sticky (rule 3), and
    // `build submit` calls this before it has any reason to have read it.
    let stored = ConfigMeta::load(cactus_root, name)?;
    let resolved = resolve_optionlist(cactus_root, name, machine, stored.as_ref(), opts)?;
    Ok(fit_from(machine, name, opts, &resolved.optionlist))
}

/// Layer `[build]`'s topology defaults onto CLI flags for a build submission:
/// CLI always wins, `[build]` fills in anything still unset. `cpus`
/// (CPUS_PER_TASK) is deliberately excluded — `reconcile_make_jobs` owns it
/// entirely, since it must stay coupled to MAKEJOBS rather than just falling
/// back to a plain default (see its doc comment).
pub fn apply_build_defaults(flags: &mut TopologyFlags, machine: &Machine) {
    let build = &machine.meta.build;
    flags.queue = flags.queue.take().or_else(|| build.queue.clone());
    flags.wall_time = flags.wall_time.or(build.walltime);
    flags.nodes = flags.nodes.or(build.nodes);
    flags.tasks = flags.tasks.or(build.tasks);
    flags.gpus_per_task = flags.gpus_per_task.or(build.gpus_per_task);
}

/// Resolve `@MAKEJOBS@` and `CPUS_PER_TASK` together for a build submission
/// (§7.6/§8.5): a build reserves `CPUS_PER_TASK` cores for its one task and
/// runs `make -j@MAKEJOBS@` inside that reservation, so the two must never
/// diverge in the dangerous direction (more make parallelism than cores).
/// Mutates `flags.cpus` so the ordinary topology chain (`resolve_topology`)
/// resolves `CPUS_PER_TASK` correctly; returns the `MAKEJOBS` value to freeze.
///
/// | given       | MAKEJOBS               | CPUS_PER_TASK                                    |
/// |-------------|-------------------------|---------------------------------------------------|
/// | -j N, no -c | N                       | N                                                   |
/// | -c N, no -j | N                       | N (unchanged)                                       |
/// | -j max      | the shell `nproc` expr | -c → `[build].cpus-per-task` → queue default → 1   |
/// | both        | as given (warn if j > cpus) | as given                                      |
/// | neither     | `[build].make-jobs`    | `[build].cpus-per-task`, else `[build].make-jobs`  |
pub fn reconcile_make_jobs(flags: &mut TopologyFlags, make_jobs: Option<MakeJobs>, machine: &Machine) -> VarValue {
    let build = &machine.meta.build;
    match (make_jobs, flags.cpus) {
        (Some(MakeJobs::Count(n)), None) => {
            flags.cpus = Some(n);
            VarValue::Int(n as i64)
        }
        (None, Some(n)) => VarValue::Int(n as i64),
        (Some(MakeJobs::Max), _) => {
            flags.cpus = flags.cpus.or(build.cpus_per_task);
            VarValue::Str(MAX_MAKEJOBS.to_owned())
        }
        (Some(MakeJobs::Count(n)), Some(c)) => {
            if n > c {
                eprintln!(
                    "{} -j {n} exceeds --cpus {c}; make may oversubscribe the reservation",
                    "warning:".yellow().bold()
                );
            }
            VarValue::Int(n as i64)
        }
        (None, None) => {
            flags.cpus = build.cpus_per_task.or(build.make_jobs);
            VarValue::Int(build.make_jobs.unwrap_or(1) as i64)
        }
    }
}

/// Everything `build submit` resolves before `prepare` runs and must freeze
/// into the attempt exactly as decided: the scheduler reservation, and the
/// `MAKEJOBS` value `reconcile_make_jobs` derives alongside it. The
/// store-submit-store order in the submit command (never lose a real job id
/// to a crash) depends on this being frozen before submission, not after.
pub struct SubmitReservation {
    pub reservation: Reservation,
    pub make_jobs: VarValue,
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

/// The outcome of the decide phase (§7.8): either there is nothing to build,
/// or a build has been staged and is ready for `execute` to run. Splitting
/// `build()` at this seam is what lets the decision run on a login node
/// while the (possibly much later, possibly elsewhere) `make` runs on a
/// compute node — see `attempt.rs`'s module doc for the on-disk shape.
// `prepare` is called at most once per `cactup build` invocation — not a hot
// loop like a listing row — so boxing either variant to shave padding off
// the other would only add indirection noise for no measurable benefit.
#[allow(clippy::large_enum_variant)]
pub enum Prepared {
    /// §7.8 short-circuit: nothing to build. Carries the (possibly
    /// baseline-refreshed) metadata, exactly as today's early return did.
    UpToDate(ConfigMeta),
    /// An attempt is staged on disk and ready to run.
    Ready(BuildAttempt),
}

/// `RebuildDecision`'s reason, as a string for `build.toml`'s `decision`
/// field — `build show` prose, not user-facing help text, so (unlike
/// `bail!`/`println!` strings) it may carry §-refs freely; it doesn't here
/// only because the reasons already do at their definition sites.
fn decision_reason(decision: &RebuildDecision) -> &'static str {
    match decision {
        RebuildDecision::Fresh => "no existing build",
        RebuildDecision::UpToDate => "up to date",
        RebuildDecision::Incremental(why) | RebuildDecision::Full(why) => why,
    }
}

/// Decide whether `name` needs a build and, if so, stage everything
/// `execute` will need into a fresh [`BuildAttempt`] (§7.8). Reads the MDB,
/// the global DB, and the installation registry — `execute` may not (D11).
///
/// CRITICAL: never mutates `configs/<name>/` itself. Today staging happened
/// milliseconds before `make` ran; once a build can be queued, `prepare` may
/// run hours before `execute` does — or never run at all, if the attempt is
/// abandoned — so every file this function writes goes into the attempt
/// directory instead. The stale-per-thorn-state deletion and the config-dir
/// skeleton creation that used to happen here are deferred to `execute` for
/// the same reason (see its doc comment).
pub fn prepare(
    installation: &Installation,
    machine: &Machine,
    name: &str,
    opts: &BuildOpts,
    submit: Option<&SubmitReservation>,
) -> Res<Prepared> {
    let cactus_root = installation.cactus_root();
    if !cactus_root.is_dir() {
        bail!("no Cactus tree at {}", cactus_root.display());
    }
    let config_dir = cactus_root.join("configs").join(name);
    // Loaded up front: it carries the thornlist this config was last built
    // from, which feeds thornlist resolution below (§7.5).
    let stored_meta = ConfigMeta::load(&cactus_root, name)?;

    // Selection & inputs (§4.4, §7.8). The universe name comes off the same
    // `fit_from` that `queue_fit` uses, so this and the pre-topology call
    // `build submit` makes before `prepare` runs can never silently disagree.
    let resolved = resolve_optionlist(&cactus_root, name, machine, stored_meta.as_ref(), opts)?;
    let ResolvedOptionlist { source: optionlist_source, optionlist, .. } = &resolved;
    if resolved.from_snapshot {
        println!(
            "{} optionlist {optionlist_source} is no longer readable; building from the copy \
             snapshotted in the config ({OPTIONLIST_SNAPSHOT}).",
            "warning:".yellow().bold(),
        );
    }
    let universe_name = fit_from(machine, name, opts, optionlist).universe;
    // Unknown universe = hard error listing the known ones (§4.8).
    let universe = universe_name
        .as_deref()
        .map(|u| machine.meta.universe(u))
        .transpose()?;
    // Frozen for `execute` (D11): a config's recorded universe is only a
    // *name* — the executing node must re-wrap the build command without
    // reading the MDB.
    let universe_spec = match (universe_name.as_deref(), universe) {
        (Some(uname), Some(u)) => Some(UniverseSpec::from_universe(uname, u)),
        _ => None,
    };

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
    // The override is silent in the processed list, so say it out loud: a
    // thorn the user put in their thornlist on purpose, dropped from the
    // config by the mdb, otherwise surfaces only as a mystery missing thorn
    // at runtime. Both layers are named separately so the warning can point
    // at the file to go edit (machine meta.toml vs. the variant's optionlist).
    warn_disabled_overrides(
        &thornlist.text,
        &machine.meta.build.disabled_thorns,
        &optionlist.header.disabled_thorns,
        &machine.name,
        optionlist_source,
    );
    let thornlist_processed =
        apply_thorn_toggles(&thornlist.text, &enabled_thorns, &disabled_thorns);

    // There is deliberately no "did the variant change out from under us?"
    // guard here any more. It existed because a bare rebuild used to
    // re-resolve to the machine's default and could silently build a
    // different flavor than the config was on record as; `resolve_optionlist`
    // now hands back whatever the config recorded (rules 3-5), so the
    // mismatch it caught can no longer arise — and the one case that still
    // can, a recorded variant the mdb has since dropped, is a hard error
    // there, where it can name what went missing.
    let flags = effective_flags(opts, stored_meta.as_ref().map(|m| m.flags));

    // Rebuild decision (§7.8): diff the SOURCE TOML snapshot, the universe, and
    // the processed thornlist.
    let snapshot_path = config_dir.join(OPTIONLIST_SNAPSHOT);
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
        .and_then(|l| crate::fetch::source_heads_with_progress(&installation.root, l).ok().flatten());
    let fresh_providers = parsed_list.as_ref().map(|l| l.thorn_providers());
    // Computed unconditionally, even under `-f`: the baseline must be
    // recorded on every build, or a config that always rebuilds with `-f`
    // could never acquire one to diff a later plain rebuild against.
    // `?` propagates only an interrupt (§2.4) — an individual unreadable
    // thorn never fails `thorn_shapes` (see its doc comment).
    let fresh_shapes = parsed_list
        .as_ref()
        .map(|l| thorn_shapes_with_progress(&cactus_root, l))
        .transpose()?;
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
        return Ok(Prepared::UpToDate(stored));
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
    if decision == RebuildDecision::Full(FLESH_NOT_AS_BUILT) {
        println!(
            "Rebuilding config {name} from scratch: the Cactus flesh is not what it was, so the \
             make system and config-data are regenerated and every existing object is stale."
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
        if !source_change.vanished.is_empty() {
            println!(
                "  no longer readable (gone, or no longer a git repo): {}",
                summarize(&source_change.vanished)
            );
        }
    }

    // Two incidents this exists for, both leaving a same-named build/<Thorn>/
    // holding state compiled from the wrong source: a provider swap (old
    // arrangement's stale `.d` files can name a bindings header the
    // reconfigure is about to delete — a hard make error) and a shape change
    // (a `.ccl` REQUIRES edit or a removed source file, same failure modes —
    // see `thorn_shapes`'s doc comment). Either way Cactus updates an
    // existing libthorn_<Thorn>.a in place with `ar`, so stale members can
    // otherwise survive into the link without so much as a warning. `Full` is
    // excluded on purpose: `realclean` already wipes every config's build
    // state, so per-thorn invalidation would just be redundant there.
    //
    // The deletion itself does NOT happen here — `execute` recomputes which
    // thorns are invalidated from its OWN re-probe (not this one) and acts on
    // it right before it actually needs the state gone (see `execute`'s doc
    // comment: this is destructive, and an attempt that never runs must never
    // have touched configs/<name>/; and a queued attempt's re-probe may find
    // a different set of changed thorns than this one did).
    let full_rebuild = !matches!(decision, RebuildDecision::Incremental(_));

    // Build-context variables (§6.3, build-time set). `submit` overrides
    // MAKEJOBS with the §7.6 coupling `build submit` already resolved
    // (against the CPUS_PER_TASK reservation it froze) — a plain foreground
    // build reserves nothing, so it keeps the ordinary flag/machine-default
    // chain.
    let mut vars = VarSet::new();
    vars.set(
        "MAKEJOBS",
        match submit {
            Some(s) => s.make_jobs.clone(),
            None => make_jobs_var(opts.make_jobs, machine.meta.build.make_jobs),
        },
    );
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
    // A future submit-script's `@CACTUP@` (mirrors the sim/testsuite paths).
    // Not read by anything in this chunk, but login-node-only, so it must be
    // frozen now — `execute` could never recover it otherwise (D11).
    let cactup = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "cactup".to_owned());
    vars.set("CACTUP", cactup);

    // Rendered native optionlist: render → inject flags → substitute (§7.8).
    let rendered = vars
        .substitute(&inject_build_flags(&optionlist.render(), flags))
        .context("substituting the rendered optionlist")?;

    // §7.7: virtual/prebuilt executable — canonicalize now (a relative path
    // would resolve against whatever directory `execute` happens to run
    // from, possibly hours later and possibly not this one).
    let virtual_executable =
        opts.virtual_executable.as_ref().map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.clone()));

    // The machine's resolved `make` invocation and build-phase env-setup —
    // both MDB-derived (`machine.meta`), so both are frozen into `BuildMeta`
    // for `execute` (D11) exactly like `universe`/`vars` are: `execute` may
    // need to drive an extra `<name>-realclean` step of its own if a re-probe
    // shows the flesh has moved since this decision was made (see its doc
    // comment), and resolving either of these afresh there would mean
    // reading the MDB. `None`/empty for a `--virtual-executable` build, which
    // never runs `make` at all.
    let (make, build_env) = if virtual_executable.is_none() {
        // The default (`DEFAULT_MAKE`) templates @MAKEJOBS@ so
        // `[build].make-jobs` (§7.6: --make-jobs > machine make-jobs > 1) is
        // honored as the default -j even on machines that don't hand-write a
        // custom `make` key. A machine that sets its own `make` keeps full
        // control of parallelism.
        let make = vars
            .substitute(machine.meta.build.make.as_deref().unwrap_or(DEFAULT_MAKE))
            .context("substituting the machine make command")?;
        // Build-phase env for the resolved universe (§6.1): universe env keys
        // override the machine [environment] key-by-key.
        let env = machine
            .meta
            .effective_env(universe_spec.as_ref().map(|u| u.name.as_str()), Phase::Build);
        (Some(make), env)
    } else {
        (None, String::new())
    };

    // The fully-formed config metadata to store on success, minus `built`
    // (execute stamps that once the build actually finishes).
    let now = Utc::now();
    let config_meta = ConfigMeta {
        schema: SCHEMA,
        name: name.to_owned(),
        // Carried forward on every rebuild, which is what makes the choice —
        // variant or file — sticky (resolution rule 3).
        optionlist_source: optionlist_source.clone(),
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
        built: None,
        flags,
        sources: fresh_sources.map(|s| s.heads),
        thorn_providers: fresh_providers,
        thorn_shapes: fresh_shapes,
    };

    let attempt_id = BuildAttempt::next_id(&config_dir)?;
    let attempt_dir = BuildAttempt::attempt_dir(&config_dir, attempt_id);
    // Frozen now, not derived later: a later `build log` reads these back off
    // the stored var set alone (D11), and the foreground path needs them too.
    vars.set("STDOUT_FILE", attempt_dir.join("build.out").display().to_string());
    vars.set("STDERR_FILE", attempt_dir.join("build.err").display().to_string());
    let meta = BuildMeta {
        schema: SCHEMA,
        attempt_id,
        config: name.to_owned(),
        optionlist_source: optionlist_source.clone(),
        machine: machine.name.clone(),
        alias: installation.alias.clone(),
        config_dir: config_dir.clone(),
        cactus_root: cactus_root.clone(),
        install_root: installation.root.clone(),
        submitted: false,
        job_id: NO_JOB_ID.to_owned(),
        status: None,
        reservation: submit.map(|s| s.reservation.clone()),
        decision: decision_reason(&decision).to_owned(),
        full_rebuild,
        make: make.clone(),
        build_env: build_env.clone(),
        virtual_executable,
        universe: universe_spec,
        config_meta,
        vars: freeze_vars(&vars),
        timestamps: Timestamps { created: Some(Utc::now()), submitted: None, started: None, finished: None },
        outcome: None,
    };
    // Creates the attempt directory and writes build.toml; from here on the
    // staged files below live under it, never under configs/<name>/.
    let attempt = BuildAttempt::create(attempt_dir, meta)?;

    // The composed build-script text, frozen for `execute` to run verbatim
    // (see its doc comment for why recomposing there would be wrong). Not
    // written at all for a virtual-executable build: that's a plain file
    // copy, not a script (see `BuildMeta::virtual_executable`'s doc comment).
    if let Some(make) = &make {
        let mut steps: Vec<String> = Vec::new();
        if matches!(decision, RebuildDecision::Full(_)) && is_configured(&cactus_root, name) {
            steps.push(format!("{make} {name}-realclean"));
        }
        steps.push(format!(
            "echo yes | {make} {name}-config options={} THORNLIST={}",
            sh_quote(&attempt.optionlist_path()),
            sh_quote(&attempt.thornlist_path()),
        ));
        if opts.clean {
            steps.push(format!("{make} {name}-clean"));
        }
        steps.push(format!("{make} {name}"));
        steps.push(format!("{make} {name}-utils"));

        // `.cactup-builds/` is safe from `make <config>-realclean`: the flesh
        // rule (Cactus/lib/make/make.configuration:298) removes only
        // `piraha`, `build`, `bindings`, `config-data/make.thornlist`, `lib`,
        // `scratch`, and `datestamp.o` — never the config dir wholesale,
        // never dot-prefixed entries — so a live build.out survives even the
        // realclean step above.
        let script = format!(
            "#!/bin/sh\nset -e\ncd {}\n{}{}\n",
            sh_quote(&cactus_root),
            if build_env.is_empty() { String::new() } else { format!("{build_env}\n") },
            steps.join("\n"),
        );
        write_executable(&attempt.script_path(), &script)?;
    }

    // Stage cactup's own files into the attempt dir (never configs/<name>/ —
    // see this function's doc comment). `execute` installs them into
    // configs/<name>/ only once the build actually succeeds.
    fs::write(attempt.optionlist_path(), &rendered)
        .with_context(|| format!("Failed to write {}", attempt.optionlist_path().display()))?;
    fs::write(attempt.optionlist_snapshot_path(), &optionlist.source)
        .with_context(|| format!("Failed to write {}", attempt.optionlist_snapshot_path().display()))?;
    fs::write(attempt.thornlist_path(), &thornlist_processed)
        .with_context(|| format!("Failed to write {}", attempt.thornlist_path().display()))?;
    // Snapshot the source verbatim, so a rebuild survives the file it came
    // from moving or being deleted (resolve_thornlist step 3). Written from
    // `thornlist.text`, not the processed copy: a rebuild must re-apply
    // whatever the machine's thorn toggles say *then*, not replay old ones.
    fs::write(attempt.thornlist_snapshot_path(), &thornlist.text)
        .with_context(|| format!("Failed to write {}", attempt.thornlist_snapshot_path().display()))?;

    Ok(Prepared::Ready(attempt))
}

/// Run a prepared attempt: acquire the per-config build lock, invoke `make`
/// (or, for `--virtual-executable`, the prebuilt-binary copy), and record the
/// outcome. Must not read the MDB, the global DB, the installation registry,
/// or knobs (D11) — everything it needs was frozen into `attempt.meta` by
/// `prepare`. `tee = true` mirrors the run to the terminal as well as
/// `build.out`/`build.err` (a foreground build, `build()`'s only caller
/// today); `tee = false` is for the future compute-node path, where the
/// scheduler owns the output files.
///
/// The config-dir skeleton and the stale-per-thorn-build-state deletion both
/// happen HERE, not in `prepare`: an attempt that is staged but never run —
/// or queued and only run hours later — must not have touched
/// `configs/<name>/` in the meantime (see `prepare`'s doc comment).
///
/// This is also where the §7.4 source/provider/shape fingerprints actually
/// get taken, NOT `prepare` (regardless of what `prepare` observed for its
/// own rebuild-DECISION purposes): a queued build can sit for hours between
/// `prepare` staging it and `execute` finally running, long enough for a
/// `cactup installation refetch`, a `git checkout`, or a hand edit to land
/// underneath it. `ConfigMeta` is documented as recording "the source state
/// this build compiled" — if that record were `prepare`'s stale copy, a
/// refetch during the queue wait would go unrecorded, the next `cactup
/// build` would diff stored-against-live, find them identical, print
/// "up to date", and never rebuild a binary that is silently stale. The
/// re-probe here is a second pass over the same ground `prepare` already
/// covered — for a foreground build (`prepare`/`execute` seconds apart) that
/// second pass finds nothing new and just costs one extra (parallel, fast)
/// walk; for a queued build it is the difference between a true record and a
/// false one. The invalidated-thorn set (`changed_providers`/
/// `changed_shapes`) is likewise recomputed from THIS probe, never trusted
/// from `prepare`'s frozen copy — see the per-thorn deletion block below.
///
/// Deliberately NOT re-run: the optionlist/thornlist/universe half of the
/// §7.8 rebuild decision (`RebuildDecision::Full`/`Incremental` from an
/// optionlist or thornlist edit). That half is genuinely MDB-derived
/// (`machine.meta`, the optionlist variant) and D11 forbids reading the MDB
/// here — it stays correctly frozen at `prepare` time. Only the
/// source-derived half (`SourceDelta`) can plausibly change while a build
/// sits queued, so only it is re-checked, via the `FLESH_NOT_AS_BUILT`
/// upgrade below.
pub fn execute(attempt: &mut BuildAttempt, tee: bool) -> Res<ConfigMeta> {
    let config_dir = attempt.meta.config_dir.clone();
    let cactus_root = attempt.meta.cactus_root.clone();
    let install_root = attempt.meta.install_root.clone();
    let name = attempt.meta.config.clone();

    // D11/queued-build safety: `configs/<name>/` and this attempt's own
    // directory both already exist by the time `prepare` returns — even for
    // a brand-new config, since `BuildAttempt::create`'s
    // `fs::create_dir_all` of `config_dir/.cactup-builds/<id>/` brings
    // `config_dir` along with it as a parent. Their absence now can
    // therefore only mean someone removed them while this attempt sat
    // queued — most likely `cactup config delete`. Refuse rather than
    // silently recreating (resurrecting) a deleted config.
    if !config_dir.is_dir() || !attempt.dir.is_dir() {
        bail!(
            "config \"{name}\" (or this build's own attempt directory, {}) no longer exists — \
             it was likely removed (e.g. by `cactup config delete`) while this build was \
             queued; refusing to recreate it",
            attempt.dir.display(),
        );
    }

    // The per-repo/per-thorn skeleton subdirectories: makes configs/<name>
    // ready for Cactus's setup_configuration.pl before it ever runs, so it
    // takes its "Reconfiguring" branch — which chdirs into the skeleton only
    // the new-config branch creates. Create that skeleton ourselves, or the
    // first configure dies with "Internal error - couldn't enter
    // '…/config-data'".
    for sub in ["build", "lib", "scratch", "config-data"] {
        fs::create_dir_all(config_dir.join(sub))
            .with_context(|| format!("Failed to create {}", config_dir.join(sub).display()))?;
    }

    // Per-config build lock, heartbeat-kept across the (long) make (§2.3 #4).
    let _build_lock = LinkLock::acquire(&config_dir.join(".cactup-build.lock"))?.with_heartbeat();
    // This attempt's own liveness marker (mirrors Restart/TestRun's
    // running.lock): a future `build show`/`build stop` reads THIS lock to
    // ask "is this attempt still running", separate from the config-wide
    // lock above, which only guards concurrent `make` invocations.
    let running = LinkLock::acquire(&attempt.running_lock_path())?.with_heartbeat();
    attempt.touch_heartbeat();

    attempt.meta.timestamps.started = Some(Utc::now());
    attempt.meta.status = Some("R".to_owned());
    attempt.store_meta()?;

    // §7.4/D11 re-probe (see this function's doc comment for why it must
    // happen here, not just at `prepare` time). The processed thornlist is
    // re-parsed from the copy `prepare` staged into the attempt dir — that,
    // `install_root`, and `cactus_root` are all already frozen in
    // `attempt.meta`, so this reads no MDB, global DB, installation
    // registry, or knob (D11-clean).
    let processed_thornlist = fs::read_to_string(attempt.thornlist_path())
        .with_context(|| format!("Failed to read {}", attempt.thornlist_path().display()))?;
    let fresh_list = crate::thornlist::parse(&processed_thornlist).ok();
    // Best-effort, matching `prepare`'s own tolerance: an unparseable
    // thornlist or a tree with no inspectable repo yields no baseline rather
    // than failing the build outright.
    let fresh_sources = fresh_list
        .as_ref()
        .and_then(|l| crate::fetch::source_heads_with_progress(&install_root, l).ok().flatten());
    let fresh_providers = fresh_list.as_ref().map(|l| l.thorn_providers());
    // Unlike `fresh_sources` above, a `thorn_shapes` failure is NOT
    // swallowed: since this chunk gave it proper interrupt support (§2.4),
    // its only failure mode is the user having hit Ctrl-C, and that must
    // abort this build rather than silently compiling against an incomplete
    // shape probe.
    let fresh_shapes = match &fresh_list {
        Some(l) => Some(thorn_shapes_with_progress(&cactus_root, l)?),
        None => None,
    };

    // What this config's LAST SUCCESSFUL build actually recorded, read fresh
    // off disk rather than from `attempt.meta.config_meta` (`prepare`'s
    // frozen copy of what IT saw there). Reading it fresh is what makes it
    // fine for another build of this same config to have completed while
    // this one sat queued: we diff against whatever baseline is on disk
    // right now and proceed — we do not refuse just because it changed
    // under us, since the user explicitly asked for THIS build.
    let previous = ConfigMeta::load(&cactus_root, &name)?;
    if let Some(prev) = &previous
        && let (Some(prev_built), Some(staged)) = (prev.built, attempt.meta.timestamps.created)
        && prev_built > staged
    {
        println!(
            "Note: config {name} was rebuilt by another `cactup build` while this one was \
             queued; proceeding anyway — this build was explicitly requested."
        );
    }
    let (sources_now, _) =
        source_delta(previous.as_ref().and_then(|m| m.sources.as_ref()), fresh_sources.as_ref());
    let changed_providers = provider_delta(
        previous.as_ref().and_then(|m| m.thorn_providers.as_ref()),
        fresh_providers.as_ref(),
    );
    let changed_shapes =
        shape_delta(previous.as_ref().and_then(|m| m.thorn_shapes.as_ref()), fresh_shapes.as_ref());

    // Upgrade an incremental build to a from-scratch one if the re-probe now
    // shows the flesh has moved since `prepare` decided — the
    // `FLESH_NOT_AS_BUILT` condition, re-checked here because a queued build
    // may have waited hours since `prepare` last looked. See this function's
    // doc comment for why only this (source-derived) half of the rebuild
    // decision is re-run.
    let flesh_escalated = !attempt.meta.full_rebuild && sources_now == SourceDelta::Flesh;
    if flesh_escalated {
        attempt.meta.full_rebuild = true;
        attempt.meta.decision = FLESH_NOT_AS_BUILT.to_owned();
        println!(
            "Escalating config {name} to a from-scratch rebuild: the Cactus flesh moved while \
             this build was queued, so the make system and config-data must be regenerated."
        );
        attempt.store_meta()?;
    }

    // Two incidents this exists for, both leaving a same-named build/<Thorn>/
    // holding state compiled from the wrong source: a provider swap (old
    // arrangement's stale `.d` files can name a bindings header the
    // reconfigure below is about to delete — a hard make error) and a shape
    // change (a `.ccl` REQUIRES edit or a removed source file, same failure
    // modes — see `thorn_shapes`'s doc comment). Either way Cactus updates an
    // existing libthorn_<Thorn>.a in place with `ar`, so stale members can
    // otherwise survive into the link without so much as a warning. Both
    // must go before make runs. `full_rebuild` is excluded on purpose:
    // `realclean` already wipes every config's build state, so this would
    // just be redundant there.
    if !attempt.meta.full_rebuild {
        let invalidated: Vec<String> = changed_providers
            .iter()
            .chain(&changed_shapes)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !invalidated.is_empty() {
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
            for thorn in &invalidated {
                remove_stale(&config_dir.join("build").join(thorn), |p| fs::remove_dir_all(p))?;
                remove_stale(&config_dir.join("lib").join(format!("libthorn_{thorn}.a")), |p| fs::remove_file(p))?;
            }
        }
    }

    let status: Option<std::process::ExitStatus> = if let Some(prebuilt) = &attempt.meta.virtual_executable {
        // §7.7: virtual/prebuilt executable — copy into place, skip make.
        // Not run through the build universe or a spawned shell: a plain
        // in-process file copy, exactly as before this split.
        let exe_dir = cactus_root.join("exe");
        fs::create_dir_all(&exe_dir).with_context(|| format!("Failed to create {}", exe_dir.display()))?;
        fs::copy(prebuilt, exe_dir.join(format!("cactus_{name}")))
            .with_context(|| format!("Failed to copy {}", prebuilt.display()))?;
        None
    } else {
        let vset = thaw_vars(&attempt.meta.vars)?;
        // A wrapper universe may hand the build to the scheduler (e.g. an
        // srun prefix), which sits silently in the queue until it gets an
        // allocation — say so up front, or the wait looks like a hang.
        if let Some(spec) = &attempt.meta.universe
            && (spec.wrapper.is_some() || spec.wrapper_argv.is_some())
        {
            println!(
                "Building inside universe \"{}\"; if its wrapper goes through the \
                 scheduler, output stays silent until the job is allocated (check the queue).",
                spec.name,
            );
        }
        let universe = attempt.meta.universe.as_ref().map(UniverseSpec::to_universe);

        // The re-probe above may have escalated this build to a from-scratch
        // rebuild after `prepare` already composed (and froze) a script with
        // no realclean step in it. Drive one of our own first, using the
        // exact `make`/env `prepare` froze for exactly this (D11: resolving
        // either afresh here would mean reading the MDB) — `is_configured`
        // mirrors `prepare`'s own gating: nothing to clean on a config that
        // was never configured to begin with.
        if flesh_escalated && is_configured(&cactus_root, &name) {
            let make = attempt.meta.make.as_deref().context(
                "a from-scratch rebuild was needed but no `make` command was frozen for this attempt",
            )?;
            let realclean_script = format!(
                "#!/bin/sh\nset -e\ncd {}\n{}{make} {name}-realclean\n",
                sh_quote(&cactus_root),
                if attempt.meta.build_env.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", attempt.meta.build_env)
                },
            );
            let realclean_path = attempt.dir.join("build-script-realclean");
            write_executable(&realclean_path, &realclean_script)?;
            let cmd = script_command(&realclean_path, universe.as_ref(), &vset, &cactus_root)?;
            let (realclean_out, realclean_err) =
                (attempt.dir.join("realclean.out"), attempt.dir.join("realclean.err"));
            let tee_files = tee.then(|| (realclean_out.clone(), realclean_err.clone()));
            let realclean_status = spawn_and_wait(cmd, &attempt.heartbeat_path(), tee_files)?;
            if !realclean_status.success() {
                bail!(
                    "the escalated realclean step failed ({realclean_status}); see {} and {}",
                    realclean_out.display(),
                    realclean_err.display(),
                );
            }
        }

        let cmd = script_command(&attempt.script_path(), universe.as_ref(), &vset, &cactus_root)?;
        let tee_files = tee.then(|| (attempt.out_path(), attempt.err_path()));
        Some(spawn_and_wait(cmd, &attempt.heartbeat_path(), tee_files)?)
    };

    drop(running);
    attempt.meta.timestamps.finished = Some(Utc::now());
    attempt.meta.status = Some("U".to_owned());

    if let Some(st) = status
        && !st.success()
    {
        attempt.meta.outcome = Some(BuildOutcomeRecord { exit_status: st.code(), complete: false });
        attempt.store_meta()?;
        eprintln!(
            "\n{} the build failed ({st})\n{} {} and {}",
            "✗".red().bold(),
            "→ build output:".red().bold(),
            attempt.out_path().display(),
            attempt.err_path().display(),
        );
        bail!(
            "the build failed ({st}); see {} and {}",
            attempt.out_path().display(),
            attempt.err_path().display(),
        );
    }

    if !is_complete(&cactus_root, &name) {
        // Report the component that is actually absent (§7.2). The two states
        // point the operator at opposite ends of the output:
        //   - configure never completed  → the marker is missing; look near
        //     the TOP of build.out/build.err (a CST/configure error).
        //   - configure done, no exe     → the compile/link failed; look near
        //     the END of build.out/build.err.
        let (missing, hint) = if !is_configured(&cactus_root, &name) {
            (
                completeness_marker(&cactus_root, &name),
                "the configure step did not complete — look near the top of build.out/build.err",
            )
        } else {
            (
                executable_path(&cactus_root, &name),
                "the compile/link step did not complete — look near the end of build.out/build.err",
            )
        };
        let (out, err) = (attempt.out_path(), attempt.err_path());
        let log_hint = if out.exists() || err.exists() {
            eprintln!(
                "\n{} the build command finished but {} is missing — the config is incomplete\n  {}\n{} {} and {}",
                "✗".red().bold(),
                missing.display(),
                hint,
                "→ build output:".red().bold(),
                out.display(),
                err.display(),
            );
            format!("; {hint}; see {} and {}", out.display(), err.display())
        } else {
            format!("; {hint}")
        };
        attempt.meta.outcome =
            Some(BuildOutcomeRecord { exit_status: status.and_then(|s| s.code()), complete: false });
        attempt.store_meta()?;
        bail!(
            "the build command finished but {} is missing — the config is incomplete{log_hint}",
            missing.display(),
        );
    }

    // Success (§7.4, §7.8): stamp `built`, store into configs/<name>/, and —
    // only now — install the processed thornlist and the optionlist source
    // snapshot there too. Writing them any earlier (i.e. in `prepare`) would
    // mean an abandoned or still-queued attempt could leave "thornlist
    // unchanged" forever, so a real change would never trigger a rebuild —
    // the whole reason this split keeps `prepare` from touching
    // configs/<name>/ (see its doc comment).
    let mut meta = attempt.meta.config_meta.clone();
    meta.built = Some(Utc::now());
    // §7.4/D11: overwrite whatever `prepare` froze here with what THIS
    // function just re-probed above — see this function's doc comment. This
    // is the whole point of the chunk: the recorded fingerprints must be
    // "the source state this build compiled", not a possibly-hours-stale
    // copy from staging time.
    meta.sources = fresh_sources.map(|s| s.heads);
    meta.thorn_providers = fresh_providers;
    meta.thorn_shapes = fresh_shapes;
    meta.store(&cactus_root)?;
    fs::copy(attempt.optionlist_path(), config_dir.join("cactup-optionlist.cfg"))
        .with_context(|| "Failed to install cactup-optionlist.cfg")?;
    fs::copy(attempt.optionlist_snapshot_path(), config_dir.join(OPTIONLIST_SNAPSHOT))
        .with_context(|| format!("Failed to install {OPTIONLIST_SNAPSHOT}"))?;
    fs::copy(attempt.thornlist_path(), config_dir.join(THORNLIST_PROCESSED))
        .with_context(|| format!("Failed to install {THORNLIST_PROCESSED}"))?;
    fs::copy(attempt.thornlist_snapshot_path(), config_dir.join(THORNLIST_SNAPSHOT))
        .with_context(|| format!("Failed to install {THORNLIST_SNAPSHOT}"))?;

    attempt.meta.outcome = Some(BuildOutcomeRecord { exit_status: status.and_then(|s| s.code()), complete: true });
    attempt.meta.config_meta = meta.clone();
    attempt.store_meta()?;

    Ok(meta)
}

/// Run `config build` for `name` (§7): `prepare` then `execute`, exactly as
/// before this split — `cactup build` still runs both back to back today;
/// only a later submit-path chunk lets time pass between them. Returns the
/// stored metadata. The global DB is never touched by `execute` (§2.3); the
/// caller updates the active-config pointer afterwards.
pub fn build(
    installation: &Installation,
    machine: &Machine,
    name: &str,
    opts: &BuildOpts,
) -> Res<BuildOutcome> {
    match prepare(installation, machine, name, opts, None)? {
        Prepared::UpToDate(meta) => Ok(BuildOutcome { meta, rebuilt: false }),
        Prepared::Ready(mut attempt) => {
            // Foreground build: this process IS the job (mirrors the
            // testsuite/sim foreground paths' `job_id = process::id()`).
            attempt.meta.job_id = std::process::id().to_string();
            attempt.store_meta()?;
            let meta = execute(&mut attempt, true)?;
            Ok(BuildOutcome { meta, rebuilt: true })
        }
    }
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
        let out = apply_thorn_toggles(list, &["CarpetX/TestReal2".into()], &[]);
        assert_eq!(out, list, "enabling must not rewrite prose that names the thorn");
        let off = apply_thorn_toggles(list, &[], &["CarpetX/TestReal2".into()]);
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
            optionlist_source: OptionlistSource::Variant("default".to_owned()),
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

    /// The loud-warning input: which thorns the thornlist asks for that the
    /// mdb takes back. Only lines the list *actively* enables count, and the
    /// layer that took it away is named so the warning can point at a file.
    #[test]
    fn disabled_overrides_only_count_thorns_the_list_actively_enables() {
        let list = "!CRL_VERSION = 1.0\n\
                    !TARGET = $ROOT\n\
                    # ExternalLibraries/PAPI is prose here, not a thorn line\n\
                    CactusBase/IOUtil\n\
                    ExternalLibraries/PAPI\n\
                    #DISABLED McLachlan/ML_BSSN\n\
                    CarpetX/CarpetX\n\
                    EinsteinInitialData/Meudon_Bin_BH\n";
        let machine = ["ExternalLibraries/PAPI".to_owned(), "ML_BSSN".to_owned()];
        let variant = ["CarpetX".to_owned(), "ExternalLibraries/PAPI".to_owned()];
        let got = disabled_thorn_overrides(list, &machine, &variant);
        assert_eq!(
            got,
            vec![
                // Matched by its full path; the prose line above it and the
                // `!` directives are not thorn lines.
                DisabledOverride {
                    thorn: "ExternalLibraries/PAPI".to_owned(),
                    spec: "ExternalLibraries/PAPI".to_owned(),
                    by: DisabledBy::Machine,
                },
                // Bare-name entry, and the variant is the only layer that
                // disables it.
                DisabledOverride {
                    thorn: "CarpetX/CarpetX".to_owned(),
                    spec: "CarpetX".to_owned(),
                    by: DisabledBy::Optionlist,
                },
            ],
            "ML_BSSN is already #DISABLED (no conflict), Meudon_Bin_BH is untouched, and PAPI \
             is attributed to the machine even though the variant disables it too"
        );
        // Every reported thorn really does get switched off downstream.
        let processed = apply_thorn_toggles(list, &[], &{
            let mut all = machine.to_vec();
            all.extend(variant.iter().cloned());
            all
        });
        for o in &got {
            assert!(
                processed.lines().any(|l| l == format!("#DISABLED {}", o.thorn)),
                "{} should be disabled in:\n{processed}",
                o.thorn
            );
        }
        assert!(disabled_thorn_overrides(list, &[], &[]).is_empty(), "no disabled-thorns, no warning");
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
            ..SourceHeads::default()
        }
    }

    /// `heads`, plus repos the live tree can no longer produce a state string
    /// for (`.git` replaced by a hand-built variant, or the directory gone).
    fn heads_with_vanished(
        pairs: &[(&str, &str)],
        flesh: Option<&str>,
        vanished: &[&str],
    ) -> SourceHeads {
        SourceHeads {
            unreadable: vanished.iter().map(|r| r.to_string()).collect(),
            ..heads(pairs, flesh)
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

    /// The git → non-git transition: swapping a checkout for a hand-built
    /// variant with no `.git/` leaves nothing to compare, and comparing only
    /// the repos the live tree *could* be read from silently reported that as
    /// "unchanged" — the loudest possible source change rendered invisible.
    #[test]
    fn a_repo_that_stopped_being_a_git_repo_is_a_change() {
        let stored: BTreeMap<String, String> = [("cactusbase", "aaa"), ("flesh", "fff")]
            .iter()
            .map(|(r, h)| (r.to_string(), h.to_string()))
            .collect();

        let swapped = heads_with_vanished(&[("flesh", "fff")], Some("flesh"), &["cactusbase"]);
        let (delta, change) = source_delta(Some(&stored), Some(&swapped));
        assert_eq!(delta, SourceDelta::Thorns);
        assert_eq!(change.vanished, vec!["cactusbase".to_string()]);
        assert!(change.moved.is_empty() && change.edited.is_empty());

        // Gone from disk entirely reads the same way — `missing` and
        // `unreadable` differ only in what the report says about them.
        let gone = SourceHeads {
            missing: ["cactusbase".to_string()].into_iter().collect(),
            ..heads(&[("flesh", "fff")], Some("flesh"))
        };
        assert_eq!(source_delta(Some(&stored), Some(&gone)).0, SourceDelta::Thorns);

        // The flesh losing its git identity earns a realclean, exactly as a
        // moved flesh commit does: nothing can vouch for the compiled objects.
        let flesh_gone = heads_with_vanished(&[("cactusbase", "aaa")], Some("flesh"), &["flesh"]);
        assert_eq!(source_delta(Some(&stored), Some(&flesh_gone)).0, SourceDelta::Flesh);

        // Bootstrap tolerance is unchanged and asymmetric: a repo the stored
        // record never knew about is not a change even when it is unreadable
        // now, since there is nothing built from it to invalidate.
        let unknown_repo = heads_with_vanished(
            &[("cactusbase", "aaa"), ("flesh", "fff")],
            Some("flesh"),
            &["llama"],
        );
        assert_eq!(source_delta(Some(&stored), Some(&unknown_repo)).0, SourceDelta::Unchanged);
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
        assert_eq!(meta.optionlist_source, OptionlistSource::Variant("default".to_owned()));
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

        // The build's stdout was teed to the first attempt's build.out.
        let build_out =
            fs::read_to_string(cactus.join("configs/sim/.cactup-builds/0000/build.out")).unwrap();
        assert!(build_out.contains("fake-make: -j4 sim-config"), "{build_out}");
        assert!(build_out.contains("fake-make: -j4 sim\n"), "{build_out}");

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

    /// `--optionlist PATH` must displace the machine's own variants
    /// entirely, not merely add to them: a build driven this way should
    /// never touch `optionlists/default.toml`'s `CC = gcc`, and the file it
    /// DID use — no `[cactup]` header, so no queue restriction and no gpu
    /// claim — should be exactly what lands in both the rendered `.cfg` and
    /// the source snapshot the next rebuild decision diffs against (§7.8).
    #[test]
    fn optionlist_flag_builds_from_the_given_file_not_the_machine_variant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, mut opts) = fake_tree(
            root,
            &format!(
                "sim-config) cd {c}/configs/sim/config-data && touch cctk_Config.h ;;\n\
                 sim) mkdir -p {c}/exe && touch {c}/exe/cactus_sim ;;",
                c = cactus.display()
            ),
        );

        // A native Cactus .cfg: unquoted RHS, no [cactup] header at all.
        let user_optionlist = root.join("my.cfg");
        fs::write(&user_optionlist, "VERSION = 9\nCC = clang\n").unwrap();
        opts.optionlist = Some(user_optionlist.clone());

        let outcome = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(outcome.rebuilt);
        let meta = &outcome.meta;
        assert_eq!(
            meta.optionlist_source,
            OptionlistSource::Optionlist(user_optionlist.display().to_string())
        );
        assert!(meta.compatible_queues.is_empty());
        assert!(!meta.gpu);

        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.starts_with("VERSION = 9\n"), "{rendered}");
        assert!(rendered.contains("CC = clang"), "{rendered}");
        assert!(!rendered.contains("gcc"), "the machine's own variant must not be used: {rendered}");

        // Byte-identical to the user's file: this is what the rebuild
        // decision diffs against on the next `cactup build`.
        let snapshot = fs::read(cactus.join("configs/sim/cactup-optionlist.toml")).unwrap();
        assert_eq!(snapshot, fs::read(&user_optionlist).unwrap());
    }

    /// `--optionlist` is sticky exactly as `--thornlist` is (§7.8 rule 3): a
    /// bare `cactup build` on a config built from a file must rebuild from
    /// that same file, not silently revert to the machine's variant, and must
    /// not need the flag repeated. An edit to the file is then picked up on
    /// the next bare rebuild — as a full rebuild, since the source diff sees
    /// it. `--variant` remains the way to move the config back onto the mdb.
    #[test]
    fn optionlist_choice_is_sticky_across_rebuilds() {
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

        let user_optionlist = root.join("my.cfg");
        fs::write(&user_optionlist, "VERSION = 9\nCC = clang\n").unwrap();
        let recorded = fs::canonicalize(&user_optionlist).unwrap().display().to_string();

        let mut first = opts_with_optionlist(&opts, &user_optionlist);
        let outcome = build(&inst, &machine, "sim", &first).unwrap();
        assert!(outcome.rebuilt);
        assert_eq!(outcome.meta.optionlist_source, OptionlistSource::Optionlist(recorded.clone()));

        // The flag is NOT repeated from here on.
        first.optionlist = None;

        // Nothing changed: the sticky path resolves to the same bytes, so the
        // build short-circuits instead of erroring on a variant mismatch.
        let again = build(&inst, &machine, "sim", &first).unwrap();
        assert!(!again.rebuilt, "a bare rebuild must not re-resolve onto the machine variant");
        assert_eq!(again.meta.optionlist_source, OptionlistSource::Optionlist(recorded.clone()));

        // Edit the file: a bare rebuild picks the edit up.
        fs::write(&user_optionlist, "VERSION = 9\nCC = clang-19\n").unwrap();
        let edited = build(&inst, &machine, "sim", &first).unwrap();
        assert!(edited.rebuilt, "an edit to the sticky optionlist must force a rebuild");
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.contains("CC = clang-19"), "{rendered}");

        // Re-supplying the flag re-points what sticks: the new file is what
        // this config is on record as being from here on, and a subsequent
        // bare rebuild follows the NEW path, not the one it first had.
        let other = root.join("other.cfg");
        fs::write(&other, "VERSION = 9\nCC = icx\n").unwrap();
        let other_recorded = fs::canonicalize(&other).unwrap().display().to_string();
        let switched = build(&inst, &machine, "sim", &opts_with_optionlist(&opts, &other)).unwrap();
        assert!(switched.rebuilt);
        assert_eq!(
            switched.meta.optionlist_source,
            OptionlistSource::Optionlist(other_recorded.clone())
        );

        // Prove the re-point took by editing only the NEW file and rebuilding
        // bare — if the old path were still sticky this would be up to date.
        fs::write(&other, "VERSION = 9\nCC = icx-2025\n").unwrap();
        let after = build(&inst, &machine, "sim", &first).unwrap();
        assert!(after.rebuilt, "the re-supplied path must be the one that sticks");
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.contains("CC = icx-2025"), "{rendered}");

        // --variant is the deliberate way back onto the machine's own, and it
        // must clear the recorded path rather than leave it lying around.
        let mut back = opts_with_optionlist(&opts, &user_optionlist);
        back.optionlist = None;
        back.variant = Some("default".to_owned());
        let reverted = build(&inst, &machine, "sim", &back).unwrap();
        assert_eq!(
            reverted.meta.optionlist_source,
            OptionlistSource::Variant("default".to_owned()),
            "--variant must displace the recorded path outright, not sit alongside it",
        );
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.contains("CC = gcc"), "{rendered}");
    }

    /// The sticky path can go away — the user deletes or moves the file they
    /// built from. That must not change what gets built: the config's own
    /// verbatim snapshot stands in, exactly as `resolve_thornlist` rule 3
    /// does for a vanished thornlist, so the config stays rebuildable.
    #[test]
    fn a_vanished_sticky_optionlist_falls_back_to_the_configs_snapshot() {
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

        let user_optionlist = root.join("my.cfg");
        fs::write(&user_optionlist, "VERSION = 9\nCC = clang\n").unwrap();
        let recorded = fs::canonicalize(&user_optionlist).unwrap().display().to_string();

        let mut sticky = opts_with_optionlist(&opts, &user_optionlist);
        assert!(build(&inst, &machine, "sim", &sticky).unwrap().rebuilt);
        sticky.optionlist = None;

        // The file the config was built from disappears.
        fs::remove_file(&user_optionlist).unwrap();

        // Force a rebuild so the snapshot actually has to be parsed and
        // rendered, not just diffed.
        sticky.force = true;
        let outcome = build(&inst, &machine, "sim", &sticky).unwrap();
        assert!(outcome.rebuilt);
        // Still on record as coming from that path, and still building the
        // user's options rather than the machine's `CC = gcc`.
        assert_eq!(outcome.meta.optionlist_source, OptionlistSource::Optionlist(recorded.clone()));
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.contains("CC = clang"), "{rendered}");
        assert!(!rendered.contains("gcc"), "{rendered}");
    }

    /// `--variant` is sticky the way `--optionlist` and `--thornlist` are: on
    /// a machine with more than one variant, a bare rebuild of a config built
    /// with `--variant cuda` rebuilds cuda — it does not re-resolve to the
    /// machine's `default = true` variant, and does not make the user repeat
    /// the flag forever. Also covers the displacement rule from the variant
    /// side: `--optionlist` replaces a recorded variant outright, the mirror
    /// of `--variant` replacing a recorded path. The one case that still
    /// refuses is a recorded variant the mdb has since dropped, which is an
    /// error naming what went missing rather than a silent fallback to the
    /// default.
    #[test]
    fn variant_choice_is_sticky_across_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (mdb, _machine, inst, _opts) = fake_tree(
            root,
            &format!(
                "sim-config) cd {c}/configs/sim/config-data && touch cctk_Config.h ;;\n\
                 sim) mkdir -p {c}/exe && touch {c}/exe/cactus_sim ;;",
                c = cactus.display()
            ),
        );

        // Give the machine a second variant, with `default` marked as the
        // implicit choice (§4.4).
        let machine_dir = root.join("mdb/fake");
        let meta = fs::read_to_string(machine_dir.join("meta.toml")).unwrap();
        fs::write(
            machine_dir.join("meta.toml"),
            meta.replace(r#"variants = ["default"]"#, r#"variants = ["default", "cuda"]"#),
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\ndefault = true\n\
             [options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/cuda.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n\
             [options]\nVERSION = \"1\"\nCC = \"nvcc\"\n",
        )
        .unwrap();
        let machine = mdb.load("fake").unwrap();

        let mut opts = BuildOpts::default_for_tests();
        opts.variant = Some("cuda".to_owned());
        let outcome = build(&inst, &machine, "sim", &opts).unwrap();
        assert_eq!(outcome.meta.optionlist_source, OptionlistSource::Variant("cuda".to_owned()));

        // The bare rebuild keeps "cuda" rather than falling to the default,
        // and finds nothing to do — the flag need not be repeated.
        let bare = BuildOpts::default_for_tests();
        let again = build(&inst, &machine, "sim", &bare).unwrap();
        assert!(!again.rebuilt, "nothing changed, so the sticky variant must be up to date");
        assert_eq!(again.meta.optionlist_source, OptionlistSource::Variant("cuda".to_owned()));
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.contains("CC = nvcc"), "{rendered}");

        // Editing the recorded variant's own file is picked up bare, too.
        fs::write(
            machine_dir.join("optionlists/cuda.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n\
             [options]\nVERSION = \"1\"\nCC = \"nvcc\"\nCXX = \"nvc++\"\n",
        )
        .unwrap();
        assert!(build(&inst, &machine, "sim", &bare).unwrap().rebuilt);
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.contains("CXX = nvc++"), "{rendered}");

        // --variant is still how you deliberately switch flavors.
        let mut to_default = BuildOpts::default_for_tests();
        to_default.variant = Some("default".to_owned());
        assert_eq!(
            build(&inst, &machine, "sim", &to_default).unwrap().meta.optionlist_source,
            OptionlistSource::Variant("default".to_owned())
        );
        // ...and that switch sticks in turn.
        assert_eq!(
            build(&inst, &machine, "sim", &bare).unwrap().meta.optionlist_source,
            OptionlistSource::Variant("default".to_owned())
        );

        // The other direction of the same rule: --optionlist displaces a
        // recorded variant just as --variant displaces a recorded path. The
        // two are alternatives, so what a config records is whichever flag
        // was passed most recently — never both, and never a fallback from
        // one to the other.
        let user_optionlist = root.join("my.cfg");
        fs::write(&user_optionlist, "VERSION = 9\nCC = clang\n").unwrap();
        let recorded = fs::canonicalize(&user_optionlist).unwrap().display().to_string();
        let displaced =
            build(&inst, &machine, "sim", &opts_with_optionlist(&bare, &user_optionlist)).unwrap();
        assert!(displaced.rebuilt);
        assert_eq!(
            displaced.meta.optionlist_source,
            OptionlistSource::Optionlist(recorded.clone()),
            "--optionlist must displace the recorded variant outright",
        );
        // And it is the path, not "default", that the next bare rebuild uses.
        assert_eq!(
            build(&inst, &machine, "sim", &bare).unwrap().meta.optionlist_source,
            OptionlistSource::Optionlist(recorded)
        );
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.contains("CC = clang"), "{rendered}");
    }

    /// A recorded variant the mdb no longer has is the one case stickiness
    /// cannot paper over: there is nothing to fall back on, since the config's
    /// snapshot is one build's rendered inputs, not a standing definition of
    /// the variant. It must name what went missing and what the machine now
    /// offers, rather than quietly building the default instead.
    #[test]
    fn a_dropped_variant_is_named_not_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (mdb, _machine, inst, _opts) = fake_tree(
            root,
            &format!(
                "sim-config) cd {c}/configs/sim/config-data && touch cctk_Config.h ;;\n\
                 sim) mkdir -p {c}/exe && touch {c}/exe/cactus_sim ;;",
                c = cactus.display()
            ),
        );

        let machine_dir = root.join("mdb/fake");
        let meta = fs::read_to_string(machine_dir.join("meta.toml")).unwrap();
        fs::write(
            machine_dir.join("meta.toml"),
            meta.replace(r#"variants = ["default"]"#, r#"variants = ["default", "cuda"]"#),
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\ndefault = true\n\
             [options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/cuda.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n\
             [options]\nVERSION = \"1\"\nCC = \"nvcc\"\n",
        )
        .unwrap();

        let mut opts = BuildOpts::default_for_tests();
        opts.variant = Some("cuda".to_owned());
        assert!(build(&inst, &mdb.load("fake").unwrap(), "sim", &opts).unwrap().rebuilt);

        // The machine drops the variant this config was built with.
        let meta_toml = machine_dir.join("meta.toml");
        let dropped = fs::read_to_string(&meta_toml)
            .unwrap()
            .replace(r#"variants = ["default", "cuda"]"#, r#"variants = ["default"]"#);
        fs::write(&meta_toml, dropped).unwrap();
        fs::remove_file(machine_dir.join("optionlists/cuda.toml")).unwrap();
        let machine = mdb.load("fake").unwrap();

        let bare = BuildOpts::default_for_tests();
        let msg = match build(&inst, &machine, "sim", &bare) {
            Err(e) => format!("{e:#}"),
            Ok(o) => panic!("expected an error, built {} instead", o.meta.optionlist_source),
        };
        assert!(msg.contains("\"cuda\""), "{msg}");
        assert!(msg.contains("no longer has"), "{msg}");
        assert!(msg.contains("default"), "must list what the machine does offer: {msg}");

        // Naming a surviving variant is the way forward.
        let mut to_default = BuildOpts::default_for_tests();
        to_default.variant = Some("default".to_owned());
        assert_eq!(
            build(&inst, &machine, "sim", &to_default).unwrap().meta.optionlist_source,
            OptionlistSource::Variant("default".to_owned())
        );
    }

    /// `BuildOpts` is not `Clone` (it is a clap struct built once per run), so
    /// tests that need several variations build them from `default_for_tests`.
    fn opts_with_optionlist(base: &BuildOpts, path: &Path) -> BuildOpts {
        let mut opts = BuildOpts::default_for_tests();
        opts.force = base.force;
        opts.optionlist = Some(path.to_owned());
        opts
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
        let attempt_dir = cactus.join("configs/sim/.cactup-builds/0000");
        let build_out = attempt_dir.join("build.out");
        let build_err = attempt_dir.join("build.err");
        // The error names both output files...
        assert!(
            err.to_string().contains(&build_out.display().to_string())
                && err.to_string().contains(&build_err.display().to_string()),
            "error should point at build.out and build.err: {err}"
        );
        // ...and build.err captured make's stderr diagnostic.
        let captured = fs::read_to_string(&build_err).unwrap();
        assert!(captured.contains("no input files"), "{captured}");
    }

    /// The regression this whole chunk exists to prevent (see `prepare`'s doc
    /// comment): staging a build must not install anything into
    /// `configs/<name>/` — not the rendered optionlist, not the processed
    /// thornlist, not their snapshots, not even the build skeleton. Only a
    /// SUCCESSFUL `execute` may do that; an attempt that is staged and then
    /// abandoned must leave the next rebuild decision exactly as it would
    /// have been had `prepare` never run.
    #[test]
    fn prepare_without_execute_leaves_the_config_dir_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        // The make command is never invoked by this test — prepare() alone
        // is under test — but fake_tree needs a body to stand up the tree.
        let (_mdb, machine, inst, opts) = fake_tree(root, "sim-config) exit 1 ;;\nsim) exit 1 ;;");

        let attempt = match prepare(&inst, &machine, "sim", &opts, None).unwrap() {
            Prepared::Ready(a) => a,
            Prepared::UpToDate(_) => panic!("a never-built config must need a build"),
        };

        // The attempt directory holds everything execute() will need...
        assert!(attempt.optionlist_path().is_file());
        assert!(attempt.optionlist_snapshot_path().is_file());
        assert!(attempt.thornlist_path().is_file());
        assert!(attempt.thornlist_snapshot_path().is_file());
        assert!(attempt.script_path().is_file());

        // ...but none of it, nor the build skeleton, has touched
        // configs/sim/ itself.
        let config_dir = cactus.join("configs/sim");
        for f in ["cactup-optionlist.cfg", "cactup-optionlist.toml", THORNLIST_PROCESSED, THORNLIST_SNAPSHOT] {
            assert!(!config_dir.join(f).exists(), "{f} must not exist before execute() runs");
        }
        for sub in ["build", "lib", "scratch", "config-data"] {
            assert!(!config_dir.join(sub).exists(), "{sub}/ must not exist before execute() runs");
        }
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

    /// The whole point of this chunk (queued-build correctness, §7.4/D11):
    /// `prepare` stages an attempt and freezes its own snapshot of the
    /// source trees, but a `git checkout`/refetch/hand-edit can land
    /// underneath it before `execute` finally runs `make` — the gap a
    /// compute-node queue wait opens. The `ConfigMeta` `execute` writes on
    /// success must record what IT sees at that later point, not `prepare`'s
    /// now-stale copy: otherwise the next `cactup build` would diff
    /// stored-against-live, find them identical, and never rebuild a binary
    /// that is silently wrong.
    #[test]
    fn execute_records_the_source_state_as_of_execute_not_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        let (_mdb, machine, inst, _opts) = fake_tree(
            root,
            &format!(
                "sim-config) cd {c}/configs/sim/config-data && touch cctk_Config.h ;;\n\
                 sim) mkdir -p {c}/exe && touch {c}/exe/cactus_sim ;;",
                c = cactus.display()
            ),
        );

        // A real CRL list with a flesh ("core") and a thorn repo
        // ("cactusbase") — source tracking reads the live tree, not a replay
        // of `fetch-state.toml`, so real repos are needed to mutate.
        let list_path = root.join("crl.th");
        fs::write(
            &list_path,
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET   = $ROOT\n!TYPE = git\n!URL = https://e.invalid/cactus.git\n\
             !NAME = core\n!CHECKOUT = Makefile lib src\n\n\
             !TARGET   = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://e.invalid/cactusbase.git\n\
             !CHECKOUT = CactusBase/Boundary\n",
        )
        .unwrap();

        let repos = cactus.join("repos");
        for name in ["core", "cactusbase"] {
            crate::fetch::git::testrepo::init(&repos.join(name));
            crate::fetch::git::testrepo::commit_file(&repos.join(name), "thorn.cc", "int a;\n");
        }

        let mut opts = BuildOpts::default_for_tests();
        opts.thornlist = Some(list_path.clone());

        let mut attempt = match prepare(&inst, &machine, "sim", &opts, None).unwrap() {
            Prepared::Ready(a) => a,
            Prepared::UpToDate(_) => panic!("a never-built config must need a build"),
        };

        // What `prepare` observed and froze, before the mutation below —
        // this is the value the bug this chunk fixes would have shipped.
        let staged = attempt.meta.config_meta.sources.clone().expect("prepare must record a baseline");

        // Simulate a refetch (or a manual checkout, or a hand edit) landing
        // in the source tree while this build sat staged/queued.
        crate::fetch::git::testrepo::commit(&repos.join("cactusbase"), "landed during the queue wait");

        // Independently compute what the tree looks like NOW, off the exact
        // processed thornlist `execute` will itself read (the copy `prepare`
        // staged into the attempt dir), so this assertion does not just
        // repeat `execute`'s own logic back at itself.
        let processed = crate::thornlist::parse(&fs::read_to_string(attempt.thornlist_path()).unwrap()).unwrap();
        let live = crate::fetch::source_heads_with_progress(&inst.root, &processed).unwrap().unwrap();

        let result = execute(&mut attempt, true).unwrap();
        let recorded = result.sources.expect("execute must record sources");
        assert_ne!(
            recorded["cactusbase"], staged["cactusbase"],
            "the record must not be prepare's stale snapshot: {recorded:?} vs {staged:?}"
        );
        assert_eq!(
            recorded["cactusbase"], live.heads["cactusbase"],
            "the record must be exactly what execute itself observed"
        );
    }

    /// D11/queued-build safety (item 4): if someone runs `cactup config
    /// delete` while a staged attempt sits in the queue, `execute` must
    /// refuse rather than silently recreating the config directory it (and
    /// its own attempt subdirectory) used to live in — `fs::create_dir_all`
    /// would otherwise resurrect a config the user explicitly deleted.
    #[test]
    fn execute_refuses_when_the_config_was_deleted_while_queued() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        // The make command is never invoked — execute() must refuse before
        // it ever gets there — but fake_tree needs a body to stand up the
        // tree.
        let (_mdb, machine, inst, opts) = fake_tree(root, "sim-config) exit 1 ;;\nsim) exit 1 ;;");

        let mut attempt = match prepare(&inst, &machine, "sim", &opts, None).unwrap() {
            Prepared::Ready(a) => a,
            Prepared::UpToDate(_) => panic!("a never-built config must need a build"),
        };

        let config_dir = cactus.join("configs/sim");
        assert!(config_dir.is_dir(), "prepare must have brought the config dir along as a parent");
        fs::remove_dir_all(&config_dir).unwrap();

        let err = match execute(&mut attempt, true) {
            Ok(_) => panic!("execute must refuse when the config directory vanished"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("sim"), "should name the config: {err}");
        assert!(
            !config_dir.exists(),
            "must refuse rather than resurrecting the deleted config directory"
        );
    }

    /// Defect A, compile-failure branch: configure produced the marker but the
    /// make exited 0 without producing the executable (a scheduler wrapper
    /// swallowed the inner make's real exit status). The incompleteness error
    /// must name the missing *executable* and point at the build output
    /// files — not blame the configure marker, which is present.
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
        let attempt_dir = cactus.join("configs/sim/.cactup-builds/0000");
        assert!(
            err.contains(&attempt_dir.join("build.out").display().to_string())
                && err.contains(&attempt_dir.join("build.err").display().to_string()),
            "should point at build.out and build.err: {err}"
        );
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
