//! The machine database (MDB) — spec §4 (layout, meta.toml model, two-layer
//! system/user resolution with name-shadowing, `hostname.regexp` /
//! `discover.py` discovery,
//! variants/queues/universes and their validation), §7.8 (optionlist TOML +
//! render), §11.2 (test-partition marking & resolution).

pub mod autodetect;
pub mod discover;
pub mod meta;
pub mod optionlist;
pub mod sync;

// Convenience re-exports for the consuming subsystems.
pub use meta::{BuildAction, Hardware, Meta, Phase, ScriptKind, Universe, WrappedCommand, HOST_UNIVERSE};
pub use optionlist::Optionlist;

use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use include_dir::{include_dir, Dir};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The built-in `generic` machine, embedded in the binary so it is always
/// available — even in a distribution build before the system MDB
/// (`~/.cactup/mdb`) has been deployed. It is the zero-match fallback for
/// unrecognized hosts and the base template for `cactup machine create`
/// (§4.3/§4.6/§4.7), so it must never be able to go missing. Dev builds read
/// the on-disk copy from the repo `mdb/` and never touch this.
static GENERIC_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/mdb/generic");

/// Materialize the embedded `generic` to `~/.cactup/mdb-builtin/<hash>/generic`
/// (once per process) and return its path, or `None` if extraction fails. It is
/// written to disk rather than served from memory because the rest of the MDB
/// machinery — matcher files, `copy_dir` in `machine create` — expects a
/// real directory. The path is keyed by a content hash of the embedded tree,
/// so a binary whose `generic` differs re-extracts, and builds that embed the
/// same one share a copy.
fn builtin_generic_dir() -> Option<PathBuf> {
    static CACHED: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHED
        .get_or_init(|| match extract_builtin_generic() {
            Ok(dir) => Some(dir),
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("Warning: failed to materialize the built-in generic machine: {e:#}")
                        .yellow()
                );
                None
            }
        })
        .clone()
}

fn extract_builtin_generic() -> Res<PathBuf> {
    let dir = crate::CACTUP_ROOT
        .join("mdb-builtin")
        .join(crate::build_info::GENERIC_HASH)
        .join("generic");
    // Extract only when absent: identical embedded contents, so a leftover copy
    // under the same hash is already correct.
    if !dir.join("meta.toml").is_file() {
        extract_embedded_dir(&GENERIC_DIR, &dir)?;
    }
    Ok(dir)
}

/// Recursively write an embedded `include_dir` tree to `dest`. Uses only the
/// stable `files`/`dirs`/`contents` API, reconstructing the tree from each
/// entry's file name (paths are relative to the embed root).
fn extract_embedded_dir(dir: &Dir, dest: &Path) -> Res<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("Failed to create {}", dest.display()))?;
    for file in dir.files() {
        let name = file.path().file_name().expect("embedded file has a name");
        let path = dest.join(name);
        std::fs::write(&path, file.contents())
            .with_context(|| format!("Failed to write {}", path.display()))?;
    }
    for sub in dir.dirs() {
        let name = sub.path().file_name().expect("embedded dir has a name");
        extract_embedded_dir(sub, &dest.join(name))?;
    }
    Ok(())
}

/// Which MDB layer a machine resolved from (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// `~/.cactup/mdb` (prod, git-cloned) or the in-repo dev `mdb/`. Read-only.
    System,
    /// `~/.cactup/machines/` — user-created/customized; wins on name clashes.
    User,
}

/// The two-layer machine database (§4.1). No top-level index (D1): machines
/// are enumerated by scanning per-machine directories for a `meta.toml`.
pub struct Mdb {
    pub system_root: PathBuf,
    pub user_root: PathBuf,
}

impl Mdb {
    /// Resolve the MDB roots: `--mdb-path` override → the build-selected
    /// system root (§2.2: a dev build reads `<project root>/mdb`, a
    /// distribution build the synced copy under `~/.cactup/mdb`, fetching it
    /// first when the throttle says so — see [`sync`]); the user overlay is
    /// always `~/.cactup/machines`. An override that carries a `GENERATION`
    /// file must be of this binary's generation; one without it (a test
    /// fixture) is taken as is. `db` supplies the `mdb-url` knob.
    //
    // D11: compute-node runs never get here, so a job never syncs (or reads
    // the global DB for the knob). `sim run --sim-dir` (sim/start.rs `run`),
    // the build `--config-dir` form (commands/build.rs `auto`/`run`) and
    // `test run --test-dir` (testsuite/run.rs `start`) all return into their
    // compute paths before anything calls `Mdb::open`.
    pub fn open(mdb_path_override: Option<&Path>, db: &crate::database::Db) -> Res<Mdb> {
        let system_root = match mdb_path_override {
            Some(path) => {
                check_root_generation(path)?;
                path.to_owned()
            }
            None if !crate::build_info::is_dist() => {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb")
            }
            None => sync::system_root(&crate::CACTUP_ROOT.join("mdb"), db, sync::Mode::Throttled)?,
        };
        Ok(Mdb { system_root, user_root: crate::CACTUP_ROOT.join("machines") })
    }

    /// Explicit roots, for tests and tools.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_roots(system_root: PathBuf, user_root: PathBuf) -> Mdb {
        Mdb { system_root, user_root }
    }

    /// All machine names with their winning layer, sorted by name.
    /// Name-shadowing is applied here: a user-MDB machine hides the
    /// system-MDB machine of the same name entirely (§4.1).
    pub fn machines(&self) -> Res<BTreeMap<String, Layer>> {
        let mut out = BTreeMap::new();
        for (root, layer) in [(&self.system_root, Layer::System), (&self.user_root, Layer::User)] {
            let entries = match std::fs::read_dir(root) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                other => other.with_context(|| format!("Failed to list MDB root {}", root.display()))?,
            };
            for entry in entries {
                let entry = entry?;
                // A machine directory is one holding a meta.toml; anything
                // else (e.g. __pycache__) is not a machine.
                if !entry.path().join("meta.toml").is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                out.insert(name, layer); // User is scanned second and wins
            }
        }
        // The embedded built-in `generic` is always enumerable, even when the
        // system MDB is not on disk. Guarded by `machine_dir` so a failed
        // extraction never lists a machine that cannot then be loaded.
        if !out.contains_key("generic") && self.machine_dir("generic").is_some() {
            out.insert("generic".to_owned(), Layer::System);
        }
        Ok(out)
    }

    /// The winning directory for `name`, if the machine exists in either layer.
    pub fn machine_dir(&self, name: &str) -> Option<(PathBuf, Layer)> {
        for (root, layer) in [(&self.user_root, Layer::User), (&self.system_root, Layer::System)] {
            let dir = root.join(name);
            if dir.join("meta.toml").is_file() {
                return Some((dir, layer));
            }
        }
        // Fall back to the embedded built-in `generic` when it is absent from
        // both real layers (a release build before the system MDB is deployed).
        // Debug builds find it in `system_root` (the repo `mdb/`) above.
        if name == "generic" {
            return builtin_generic_dir().map(|dir| (dir, Layer::System));
        }
        None
    }

    /// Load, validate, autodetect-fill, and path-substitute machine `name`.
    pub fn load(&self, name: &str) -> Res<Machine> {
        let (dir, layer) = self
            .machine_dir(name)
            .ok_or_else(|| anyhow!("no machine named \"{name}\" in the MDB (try `cactup machine list`)"))?;
        Machine::load(name, dir, layer, &self.generations_guide())
    }

    /// Where a user reads what changed between MDB generations: the
    /// `GENERATIONS.md` shipped in the system MDB when it has one, else the
    /// documentation site.
    pub fn generations_guide(&self) -> String {
        let local = self.system_root.join("GENERATIONS.md");
        if local.is_file() {
            local.display().to_string()
        } else {
            format!("{}/authors/mdb-generations.html", crate::update::DEFAULT_UPDATE_URL)
        }
    }

    /// Gate a user-MDB overlay's `meta.toml` on its recorded generation
    /// without loading the machine (the `--from-existing` base check in
    /// `machine create`). Loading a user-layer machine applies the same gate.
    pub fn check_overlay(&self, name: &str, dir: &Path) -> Res<()> {
        let meta_path = dir.join("meta.toml");
        let text = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("Failed to read {}", meta_path.display()))?;
        check_overlay_generation(name, &meta_path, &text, &self.generations_guide())?;
        Ok(())
    }

    /// Every machine that claims `hostname`, sorted by name (§4.3).
    /// Shadowing is applied first, so an overridden machine never
    /// double-matches against its system original. Per machine,
    /// `hostname.regexp` is consulted first (in Rust); `discover.py` only
    /// when there is no regexp or it did not match — and all of those run
    /// in one `python3`. A machine with neither file is simply not
    /// discoverable (`--machine` still selects it); one whose discover.py
    /// raises is treated as "did not match", with a warning under `verbose`.
    pub fn discover(&self, hostname: &str, verbose: bool) -> Res<Vec<String>> {
        let mut matches = Vec::new();
        let mut pending: Vec<(String, PathBuf)> = Vec::new();
        for (name, _layer) in self.machines()? {
            let (dir, _) = self.machine_dir(&name).expect("machine enumerated but no dir");
            match discover::regexp_verdict(&name, &dir, hostname) {
                Some(true) => matches.push(name),
                Some(false) | None => {
                    let discover_py = dir.join("discover.py");
                    if discover_py.is_file() {
                        pending.push((name, discover_py));
                    }
                }
            }
        }
        let scripts: Vec<PathBuf> = pending.iter().map(|(_, path)| path.clone()).collect();
        for ((name, _), verdict) in pending.into_iter().zip(discover::probe_all(&scripts, hostname)?) {
            match verdict {
                Ok(true) => matches.push(name),
                Ok(false) => {}
                Err(e) => {
                    if verbose {
                        eprintln!(
                            "{}",
                            format!("Warning: treating machine {name} as non-matching: {e:#}").yellow()
                        );
                    }
                }
            }
        }
        matches.sort();
        Ok(matches)
    }

    /// Does machine `name` (alone) claim `hostname`? The same regexp-first,
    /// discover.py-second test `discover` applies, for re-verifying a cached
    /// machine (§4.3). A machine that no longer exists does not claim
    /// anything.
    pub fn verify(&self, name: &str, hostname: &str, verbose: bool) -> Res<bool> {
        let Some((dir, _)) = self.machine_dir(name) else { return Ok(false) };
        if discover::regexp_verdict(name, &dir, hostname) == Some(true) {
            return Ok(true);
        }
        let discover_py = dir.join("discover.py");
        if !discover_py.is_file() {
            return Ok(false);
        }
        match discover::is_machine(&discover_py, hostname) {
            Ok(claims) => Ok(claims),
            Err(e) => {
                if verbose {
                    eprintln!(
                        "{}",
                        format!("Warning: treating machine {name} as non-matching: {e:#}").yellow()
                    );
                }
                Ok(false)
            }
        }
    }
}

/// Parse the contents of an MDB `GENERATION` file: one positive integer.
pub fn parse_generation(text: &str) -> Option<u32> {
    text.trim().parse().ok().filter(|&n| n >= 1)
}

/// The generation recorded in `<root>/GENERATION`, or `None` when the file
/// does not exist. A file that exists but does not hold a generation is an
/// error.
pub fn read_generation(root: &Path) -> Res<Option<u32>> {
    let path = root.join("GENERATION");
    let text = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        other => other.with_context(|| format!("Failed to read {}", path.display()))?,
    };
    match parse_generation(&text) {
        Some(generation) => Ok(Some(generation)),
        None => {
            bail!("{} does not hold an MDB generation (one positive integer): {text:?}", path.display())
        }
    }
}

/// `--mdb-path` points at a system MDB: when it says which generation it is,
/// that must be this binary's. A tree without `GENERATION` (a test fixture)
/// is accepted silently.
fn check_root_generation(root: &Path) -> Res<()> {
    let want = crate::build_info::MDB_GENERATION;
    match read_generation(root)? {
        Some(found) if found != want => bail!(
            "the MDB at {} is generation {found}, but this cactup reads generation {want}; \
             point --mdb-path at a generation-{want} MDB{}",
            root.display(),
            if found > want { " or run `cactup update`" } else { "" }
        ),
        _ => Ok(()),
    }
}

/// A user-MDB overlay written for a different MDB generation than this
/// cactup reads (its `[cactup] mdb-generation`). An older overlay needs the
/// changes `GENERATIONS.md` lists for each newer generation; a newer one
/// needs a newer cactup.
#[derive(Debug)]
pub struct OverlayGenerationError {
    /// The machine name.
    pub name: String,
    /// The overlay's `meta.toml`.
    pub path: PathBuf,
    /// The generation the overlay records.
    pub found: u32,
    /// This binary's generation.
    pub want: u32,
    /// Where the generation changes are described: a local `GENERATIONS.md`
    /// or the documentation page.
    pub guide: String,
}

impl OverlayGenerationError {
    /// The short form for a one-line listing (`machine list`).
    pub fn brief(&self) -> String {
        if self.found < self.want {
            format!(
                "written for MDB generation {}, this cactup reads {}; `cactup machine show {}` explains",
                self.found, self.want, self.name
            )
        } else {
            format!("needs MDB generation {}; run `cactup update`", self.found)
        }
    }
}

impl std::fmt::Display for OverlayGenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { name, path, found, want, guide } = self;
        if found < want {
            write!(
                f,
                "user-MDB machine \"{name}\" ({}) was written for MDB generation {found}, but this cactup \
                 reads generation {want}. Bring it forward as described in {guide} and then set \
                 `mdb-generation = {want}` under [cactup], or re-create it with `cactup machine delete {name}` \
                 followed by `cactup machine create {name}`.",
                path.display()
            )
        } else {
            write!(
                f,
                "user-MDB machine \"{name}\" ({}) was written for MDB generation {found}, which requires a \
                 newer cactup than this one (generation {want}); run `cactup update`.",
                path.display()
            )
        }
    }
}

impl std::error::Error for OverlayGenerationError {}

/// Gate a user-MDB overlay on the generation its raw `meta.toml` records.
/// A missing `[cactup] mdb-generation` is assumed to be this binary's, with
/// a warning printed once per machine per process. Text that does not parse,
/// or a value that is not a generation, passes: the typed parse that follows
/// reports it properly.
fn check_overlay_generation(
    name: &str,
    meta_path: &Path,
    text: &str,
    guide: &str,
) -> Result<(), OverlayGenerationError> {
    let want = crate::build_info::MDB_GENERATION;
    let Ok(table) = text.parse::<toml::Table>() else { return Ok(()) };
    let recorded = table.get("cactup").and_then(|c| c.get("mdb-generation"));
    let found = match recorded {
        Some(toml::Value::Integer(n)) => match u32::try_from(*n) {
            Ok(n) => n,
            Err(_) => return Ok(()),
        },
        Some(_) => return Ok(()),
        None => {
            static WARNED: std::sync::Mutex<std::collections::BTreeSet<String>> =
                std::sync::Mutex::new(std::collections::BTreeSet::new());
            let first = WARNED.lock().map(|mut warned| warned.insert(name.to_owned())).unwrap_or(false);
            if first {
                eprintln!(
                    "{}",
                    format!(
                        "Warning: user-MDB machine \"{name}\" ({}) records no MDB generation; assuming \
                         generation {want}. Add `mdb-generation = {want}` under [cactup] to silence this.",
                        meta_path.display()
                    )
                    .yellow()
                );
            }
            return Ok(());
        }
    };
    if found == want {
        return Ok(());
    }
    Err(OverlayGenerationError {
        name: name.to_owned(),
        path: meta_path.to_owned(),
        found,
        want,
        guide: guide.to_owned(),
    })
}

/// A loaded, validated machine.
#[derive(Debug)]
pub struct Machine {
    pub name: String,
    pub dir: PathBuf,
    pub layer: Layer,
    pub meta: Meta,
}

impl Machine {
    fn load(name: &str, dir: PathBuf, layer: Layer, generations_guide: &str) -> Res<Machine> {
        let meta_path = dir.join("meta.toml");
        let text = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("Failed to read {}", meta_path.display()))?;
        // An overlay of another generation may not even parse under this
        // binary's closed schema, so its generation is checked first, on the
        // raw table, to report the real problem.
        if layer == Layer::User {
            check_overlay_generation(name, &meta_path, &text, generations_guide)?;
        }
        let mut meta: Meta = toml::from_str(&text)
            .with_context(|| format!("Failed to parse {}", meta_path.display()))?;
        meta.validate(name)?;

        // §4.6: fill missing hardware from the OS when asked to (or when some
        // queue would otherwise resolve no value — hardware may live at the
        // top level, per-queue, or both, per-queue winning; §4.2). Explicit
        // values always win, and a machine whose queues fully cover a key is
        // left alone even when the top-level key is absent.
        //
        // `max-gpus-per-node` and `threads-per-cpu` are filled opportunistically
        // but deliberately do NOT join the `incomplete` test: most machines have
        // no GPUs and no SMT worth declaring, so a missing value there is the
        // normal case, not a gap to repair — and §8.5 already falls back to one
        // GPU per task and one thread per CPU without them.
        let incomplete = meta.queues.values().any(|q| {
            q.max_cpus_per_node.or(meta.hardware.max_cpus_per_node).is_none() || q.memory.or(meta.hardware.memory).is_none()
        });
        if meta.hardware.autodetect || incomplete {
            let detected = autodetect::detect();
            let hw = &mut meta.hardware;
            hw.max_cpus_per_node = hw.max_cpus_per_node.or(Some(detected.cores));
            hw.memory = hw.memory.or(detected.memory_mb);
            hw.max_gpus_per_node = hw.max_gpus_per_node.or(detected.gpus);
            hw.threads_per_cpu = hw.threads_per_cpu.or(detected.threads_per_cpu);
        }

        // [paths] values keep their @USER@/@ENV(NAME)@ tokens at load; they
        // are resolved by Meta::resolved_paths at use time, on the machine
        // itself, so entries for machines whose environment this host lacks
        // (e.g. TACC's $SCRATCH) still load and validate everywhere (§4.2).

        let machine = Machine { name: name.to_owned(), dir, layer, meta };
        machine.validate_files()?;
        Ok(machine)
    }

    /// §4.2's layout obligations: every declared variant must have its file.
    fn validate_files(&self) -> Res<()> {
        for kind in [ScriptKind::Submit, ScriptKind::Run, ScriptKind::BuildSubmit] {
            for variant in self.meta.script_variants(kind).variants.keys() {
                self.script_path(kind, variant)?;
            }
        }
        for variant in &self.meta.variants.optionlist.variants {
            let path = self.optionlist_path(variant);
            if !path.is_file() {
                bail!(
                    "machine \"{}\" lists optionlist variant \"{variant}\" but {} does not exist",
                    self.name,
                    path.display()
                );
            }
        }
        Ok(())
    }

    pub fn optionlist_path(&self, variant: &str) -> PathBuf {
        self.dir.join("optionlists").join(format!("{variant}.toml"))
    }

    /// The script file for a variant: `<kind>/<variant>.sh` or `.py` (§4.4).
    /// Exactly one of the two must exist.
    pub fn script_path(&self, kind: ScriptKind, variant: &str) -> Res<ScriptFile> {
        let dir = self.dir.join(kind.dir_name());
        let sh = dir.join(format!("{variant}.sh"));
        let py = dir.join(format!("{variant}.py"));
        match (sh.is_file(), py.is_file()) {
            (true, false) => Ok(ScriptFile { path: sh, python: false }),
            (false, true) => Ok(ScriptFile { path: py, python: true }),
            (true, true) => bail!(
                "machine \"{}\" has both {} and {} — a variant may be .sh or .py, not both",
                self.name,
                sh.display(),
                py.display()
            ),
            (false, false) => bail!(
                "machine \"{}\" declares {} variant \"{variant}\" but neither {} nor {} exists",
                self.name,
                kind.dir_name(),
                sh.display(),
                py.display()
            ),
        }
    }

    /// Select the optionlist variant for a build (§4.4): explicit `--variant`
    /// if given; else the sole listed variant; else the sole `default = true`
    /// variant.
    pub fn select_optionlist(&self, explicit: Option<&str>) -> Res<String> {
        let listed = &self.meta.variants.optionlist.variants;

        if let Some(name) = explicit {
            return listed
                .iter()
                .find(|n| *n == name)
                .cloned()
                .ok_or_else(|| anyhow!("no optionlist variant named \"{name}\" (known: {})", listed.join(", ")));
        }

        if let [single] = listed.as_slice() {
            return Ok(single.clone());
        }
        let mut defaults = Vec::new();
        for variant in listed {
            if optionlist::load_header(&self.optionlist_path(variant))?.default {
                defaults.push(variant);
            }
        }
        match defaults.as_slice() {
            [single] => Ok((*single).clone()),
            [] if listed.is_empty() => bail!("machine \"{}\" has no optionlist variant", self.name),
            [] => bail!(
                "machine \"{}\" has several optionlist variants ({}) and none is marked \
                 default = true; pick one with --variant", // §4.4
                self.name,
                listed.join(", ")
            ),
            several => bail!(
                "machine \"{}\" marks several optionlist variants default = true ({}); \
                 pick one with --variant", // §4.4
                self.name,
                several.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// A resolved script variant file.
#[derive(Debug, Clone)]
pub struct ScriptFile {
    pub path: PathBuf,
    /// `.py` variants emit the script on stdout per the §6.1 convention.
    pub python: bool,
}

/// The invoking user, for `@USER@` substitution (§4.2, §10).
pub fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn dev_mdb() -> Mdb {
        let system = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb");
        Mdb::with_roots(system, PathBuf::from("/nonexistent-user-mdb"))
    }

    #[test]
    fn enumerates_dev_machines_skipping_pycache() {
        let machines = dev_mdb().machines().unwrap();
        for name in ["generic", "mel5", "mike"] {
            assert!(machines.contains_key(name), "dev mdb should list {name}");
        }
        assert!(!machines.keys().any(|n| n.contains("pycache")));
        assert!(machines.values().all(|l| *l == Layer::System));
    }

    /// Sweep the whole dev MDB (the simfactory2 port): every machine must
    /// load+validate (meta.toml schema, queue coverage, per-variant files),
    /// every optionlist variant must parse under the §7.8 rules, and every
    /// declared script variant must resolve to exactly one .sh/.py file.
    #[test]
    fn loads_and_validates_every_dev_machine() {
        let mdb = dev_mdb();
        for (name, _layer) in mdb.machines().unwrap() {
            let machine = mdb
                .load(&name)
                .unwrap_or_else(|e| panic!("machine {name} failed to load: {e:#}"));
            for variant in &machine.meta.variants.optionlist.variants {
                optionlist::Optionlist::load(&machine.optionlist_path(variant))
                    .unwrap_or_else(|e| panic!("machine {name} optionlist {variant} failed: {e:#}"));
            }
        }
    }

    #[test]
    fn loads_and_validates_the_real_machines() {
        let mdb = dev_mdb();

        let mel5 = mdb.load("mel5").unwrap();
        assert_eq!(mel5.meta.default_queue(HOST_UNIVERSE), Some("local"));
        // [paths] keep their tokens at load (resolution is a use-time
        // concern — §4.2)…
        assert_eq!(mel5.meta.paths.simulation_home.as_deref(), Some("/home/@USER@/simulations"));
        // …and resolve on demand.
        let user = whoami();
        let paths = mel5.meta.resolved_paths().unwrap();
        assert_eq!(paths.simulation_home.as_deref(), Some(format!("/home/{user}/simulations").as_str()));
        assert_eq!(paths.test_home.as_deref(), Some(format!("/home/{user}/tests").as_str()));
        // Scheduler templates also keep their tokens for use time.
        assert!(mel5.meta.scheduler.submit.as_deref().unwrap().contains("@SCRIPTFILE@"));
        // env-setup lives in [environment] and spans all three phases.
        assert!(mel5.meta.environment.effective(Phase::Build).contains("MPI_DIR"));
        // Optionlist selection (§4.4): with two variants, the default-marked
        // one wins implicitly and the other stays reachable via --variant.
        assert_eq!(mel5.select_optionlist(None).unwrap(), "default");
        assert_eq!(mel5.select_optionlist(Some("debug")).unwrap(), "debug");
        // Script selection honors the test partition.
        let rs = mel5.meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("local", "host", false, None).unwrap().0, "default");
        assert_eq!(rs.select("local", "host", true, None).unwrap().0, "test");
        assert!(!mel5.script_path(ScriptKind::Submit, "test").unwrap().python);

        // db1.hpc.lsu.edu is ONE machine for the whole Deep Bayou cluster: the
        // upstream fragmentation into db1 (native) + db-sing-nv + db-sing-cpu +
        // etworkshop-db is unified here via ONE real queue plus per-variant
        // `build-universes` compatibility lists routing each build flavor's
        // configs to its scripts (§4.4; self-named "db").
        let db1 = mdb.load("db1.hpc.lsu.edu").unwrap();
        assert_eq!(db1.meta.machine.name.as_deref(), Some("db"));
        assert_eq!(db1.meta.default_queue(HOST_UNIVERSE), Some("gpu"));
        // The cluster's single real partition, GPU-flagged.
        assert_eq!(db1.meta.queues.len(), 1);
        assert!(db1.meta.queues["gpu"].gpu);
        assert_eq!(db1.meta.scheduler_queue_name("gpu").unwrap(), "gpu");
        // Availability vs. request (§8.5): a Deep Bayou node HAS 48 CPUs/cores,
        // and a no-`-c` job REQUESTS 24 CPUs/task — so the fill-the-node default
        // lands on floor(48/24) = 2 tasks/node, one per GPU.
        let hw = db1.meta.effective_hardware("gpu").unwrap();
        assert_eq!(hw.max_cpus_per_node, Some(48));
        assert_eq!(hw.default_cpus_per_task, Some(24));
        // Three optionlists, none default-marked: a build must pick one with
        // --variant (§4.4).
        assert!(db1.select_optionlist(None).is_err());
        for v in ["native", "sing-nv", "sing-cpu"] {
            assert_eq!(db1.select_optionlist(Some(v)).unwrap(), v);
        }
        // The native flavor builds in the DECLARED host universe — an identity
        // universe (§4.8) carrying only a module-loading env-build-setup
        // override (§6.1) — reached via the declared-host fallback, so
        // [build].universe is unset; the Singularity flavors name their own
        // build universe in the optionlist header, --nv vs not.
        assert!(db1.meta.build.universe.is_none());
        let host = db1.meta.universe("host").unwrap();
        assert!(host.wrapper.is_none() && host.wrapper_argv.is_none());
        assert!(host.environment.env_build_setup.as_deref().unwrap().contains("module load gcc/9.3.0"));
        assert_eq!(optionlist::load_header(&db1.optionlist_path("sing-nv")).unwrap().universe.as_deref(), Some("et-sing"));
        assert_eq!(optionlist::load_header(&db1.optionlist_path("sing-cpu")).unwrap().universe.as_deref(), Some("et-sing-cpu"));
        for v in ["sing-nv", "sing-cpu"] {
            assert_eq!(optionlist::load_header(&db1.optionlist_path(v)).unwrap().compatible_queues, ["gpu"]);
        }
        let argv = |u: &str| db1.meta.universe(u).unwrap().wrapper_argv.clone().unwrap();
        assert!(argv("et-sing").iter().any(|a| a == "--nv"));
        assert!(!argv("et-sing-cpu").iter().any(|a| a == "--nv"));
        // Native builds run with the "default" scripts; the Singularity
        // flavors share the "sing" scripts — same queue, routed by the
        // config's build universe (§4.4).
        let rs = db1.meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("gpu", "host", false, None).unwrap().0, "default");
        assert_eq!(rs.select("gpu", "et-sing", false, None).unwrap().0, "sing");
        assert_eq!(rs.select("gpu", "et-sing-cpu", true, None).unwrap().0, "sing-test");

        // qbd (LSU/LONI Queen Bee 4) runs SLURM, though upstream's 2026-07-07
        // regeneration flipped it to PBS with a blank queue: the port keeps
        // #SBATCH/sbatch, matching its sibling LONI machine qbc, and replaces the
        // placeholder queue with QB4's two real GPU partitions.
        let qbd = mdb.load("qbd").unwrap();
        assert!(qbd.meta.scheduler.submit.as_deref().unwrap().starts_with("sbatch"));
        assert_eq!(qbd.meta.default_queue(HOST_UNIVERSE), Some("gpu2"));
        assert!(qbd.meta.queues["gpu2"].gpu && qbd.meta.queues["gpu4"].gpu);
        // No per-queue `name` override any more: @QUEUE@ is the key itself, so
        // the submitscripts' `-p` directive and their `QUEUE == "gpu4"` gres
        // branch both see the real partition names.
        for q in ["gpu2", "gpu4"] {
            assert_eq!(qbd.meta.scheduler_queue_name(q).unwrap(), q);
        }
        // The hardware behind the §8.5 fill-the-node layout: a 64-CPU node and a
        // 32-CPU default request = 2 tasks/node, gpu2's 32-CPUs-per-GPU cap.
        let hw = qbd.meta.effective_hardware("gpu2").unwrap();
        assert_eq!((hw.max_cpus_per_node, hw.default_cpus_per_task), (Some(64), Some(32)));
        assert_eq!(hw.max_gpus_per_node, Some(2));
        // gpu4 is the same node with twice the GPUs, so it overrides BOTH the
        // GPU count and the CPU request that sets the rank count — 64/4 = 16,
        // i.e. 4 ranks, one per GPU. Inheriting 32 would leave 2 GPUs idle;
        // `qbd_defaults_fill_each_partition` (sim::vars) pins the consequence.
        let hw = qbd.meta.effective_hardware("gpu4").unwrap();
        assert_eq!(
            (hw.max_cpus_per_node, hw.default_cpus_per_task, hw.max_gpus_per_node),
            (Some(64), Some(16), Some(4))
        );
        // QB4 makes you compile on the compute nodes: pin that the migration
        // actually wired up `build submit` (a [variants.buildsubmitscript]
        // variant, a [scheduler].submit command) and defaults to it, rather
        // than merely parsing.
        assert!(qbd.meta.can_submit_builds());
        assert_eq!(qbd.meta.build.default_action, Some(BuildAction::Submit));

        // graham unifies one Compute Canada cluster's CPU (g++) and CUDA (nvcc)
        // build flavors into two optionlist variants. The CUDA variant carries
        // per-variant disabled-thorns (§7.8) — thorns nvcc can't compile — that
        // the CPU variant keeps; this is what lets the two flavors coexist on
        // one machine.
        let graham = mdb.load("graham").unwrap();
        assert!(!graham.meta.queues["cpu"].gpu && graham.meta.queues["gpu"].gpu);
        // Compute Canada schedules by account, not partition, so both queues keep
        // the historical `-p NO_QUEUE` value — the remaining real-machine user of
        // the per-queue `name` override (§4.2).
        for q in ["cpu", "gpu"] {
            assert_eq!(graham.meta.scheduler_queue_name(q).unwrap(), "NO_QUEUE");
        }
        let gpu_ol = optionlist::load_header(&graham.optionlist_path("gpu")).unwrap();
        assert!(gpu_ol.disabled_thorns.iter().any(|t| t == "ExternalLibraries/LORENE"));
        assert!(optionlist::load_header(&graham.optionlist_path("default")).unwrap().disabled_thorns.is_empty());

        // frontera's [paths] read TACC's hashed storage roots via @ENV()@
        // (§4.2): the entry loads anywhere; resolution needs the machine's
        // own environment and hard-errors without it.
        let frontera = mdb.load("frontera").unwrap();
        assert_eq!(
            frontera.meta.paths.simulation_home.as_deref(),
            Some("@ENV(SCRATCH)@/simulations")
        );
        if std::env::var("SCRATCH").is_err() {
            let err = format!("{:#}", frontera.meta.resolved_paths().unwrap_err());
            assert!(err.contains("SCRATCH"), "error names the env var: {err}");
        }

        let generic = mdb.load("generic").unwrap();
        // §4.6: autodetect filled the missing hardware.
        assert!(generic.meta.hardware.max_cpus_per_node.unwrap() >= 1);
        assert!(generic.meta.hardware.memory.unwrap() > 0);
        // generic has no paths: consumers use the ~/.cactup fallbacks.
        assert!(generic.meta.paths.simulation_home.is_none());
        assert_eq!(generic.select_optionlist(None).unwrap(), "default"); // sole variant
    }

    #[test]
    fn user_layer_shadows_system_layer() {
        let system = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let minimal = r#"
            [machine]
            nickname = "m"
            [queues.local]
            default = true
            [variants.submitscript]
            "default" = ["local"]
            [variants.runscript]
            "default" = ["local"]
            [variants.optionlist]
            variants = ["default"]
        "#;
        for (root, tag) in [(system.path(), "system"), (user.path(), "user")] {
            let dir = root.join("box");
            fs::create_dir_all(dir.join("optionlists")).unwrap();
            fs::create_dir_all(dir.join("runscripts")).unwrap();
            fs::create_dir_all(dir.join("submitscripts")).unwrap();
            fs::write(dir.join("meta.toml"), minimal.replace("\"m\"", &format!("\"{tag}\""))).unwrap();
            fs::write(dir.join("optionlists/default.toml"), "[options]\nVERSION = \"1\"\n").unwrap();
            fs::write(dir.join("runscripts/default.sh"), "#!/bin/sh\n").unwrap();
            fs::write(dir.join("submitscripts/default.sh"), "#!/bin/sh\n").unwrap();
        }
        // Only-system machine to prove both layers are scanned.
        let solo = system.path().join("solo");
        fs::create_dir_all(&solo).unwrap();
        fs::write(solo.join("meta.toml"), minimal).unwrap();

        let mdb = Mdb::with_roots(system.path().to_owned(), user.path().to_owned());
        let machines = mdb.machines().unwrap();
        // `generic` is always present via the embedded built-in fallback, even
        // though neither temp root defines it.
        assert_eq!(machines.keys().cloned().collect::<Vec<_>>(), ["box", "generic", "solo"]);
        assert_eq!(machines["box"], Layer::User);
        assert_eq!(machines["generic"], Layer::System);
        assert_eq!(machines["solo"], Layer::System);

        let loaded = mdb.load("box").unwrap();
        assert_eq!(loaded.layer, Layer::User);
        assert_eq!(loaded.meta.machine.nickname.as_deref(), Some("user"));
    }

    #[test]
    fn load_reports_missing_variant_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("broken");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("meta.toml"),
            r#"
                [machine]
                nickname = "broken"
                [queues.local]
                default = true
                [variants.submitscript]
                "default" = ["local"]
                [variants.runscript]
                "default" = ["local"]
                [variants.optionlist]
                variants = ["default"]
            "#,
        )
        .unwrap();
        let mdb = Mdb::with_roots(root.path().to_owned(), PathBuf::from("/nonexistent"));
        let err = format!("{:#}", mdb.load("broken").unwrap_err());
        assert!(err.contains("default"), "unexpected error: {err}");
    }

    #[test]
    fn discovery_sweep_matches_mel5_only() {
        let mdb = dev_mdb();
        assert_eq!(mdb.discover("melete05.cct.lsu.edu", false).unwrap(), ["mel5"]);
        assert_eq!(mdb.discover("melete05", false).unwrap(), ["mel5"]);
        assert_eq!(mdb.discover("mike3", false).unwrap(), ["mike"]);
        assert!(mdb.discover("someone-elses-laptop", false).unwrap().is_empty());
        assert!(mdb.verify("mel5", "melete05.cct.lsu.edu", false).unwrap());
        assert!(!mdb.verify("mel5", "mike3.hpc.lsu.edu", false).unwrap());
        assert!(!mdb.verify("no-such-machine", "melete05", false).unwrap());
    }

    /// Every shipped matcher is a regexp (no python on the discovery path),
    /// every regexp compiles, and the two undiscoverable machines ship no
    /// matcher at all (§4.3, §4.6).
    #[test]
    fn every_dev_machine_regexp_compiles() {
        let mdb = dev_mdb();
        for (name, _) in mdb.machines().unwrap() {
            let (dir, _) = mdb.machine_dir(&name).unwrap();
            assert!(!dir.join("discover.py").exists(), "{name} still ships a discover.py");
            let regexp = dir.join("hostname.regexp");
            if matches!(name.as_str(), "generic" | "et-juphub") {
                assert!(!regexp.exists(), "{name} must not be discoverable");
                continue;
            }
            discover::load_regexp(&regexp).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
    }

    /// The regexp is the fast path: a matching regexp means the machine's
    /// discover.py is never run; a non-matching one hands over to it.
    #[test]
    fn regexp_short_circuits_discover_py() {
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let system = tempfile::tempdir().unwrap();
        let sentinel = system.path().join("ran");
        let dir = system.path().join("box");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(dev_mdb().system_root.join("generic/meta.toml"), dir.join("meta.toml")).unwrap();
        std::fs::write(dir.join("hostname.regexp"), "^fast\\.example$\n").unwrap();
        std::fs::write(
            dir.join("discover.py"),
            format!(
                "import pathlib\npathlib.Path({:?}).write_text('x')\ndef is_machine(h):\n    return h == 'slow.example'\n",
                sentinel.display().to_string()
            ),
        )
        .unwrap();
        let mdb = Mdb::with_roots(system.path().to_owned(), tempfile::tempdir().unwrap().path().to_owned());
        assert_eq!(mdb.discover("fast.example", false).unwrap(), ["box"]);
        assert!(!sentinel.exists(), "regexp matched, python must not run");
        assert_eq!(mdb.discover("slow.example", false).unwrap(), ["box"]);
        assert!(sentinel.exists(), "regexp missed, discover.py decides");
        assert!(mdb.discover("neither.example", false).unwrap().is_empty());
    }

    /// A loadable machine `name` under `root` whose meta.toml ends with `extra`.
    fn write_minimal_machine(root: &Path, name: &str, extra: &str) {
        let dir = root.join(name);
        for sub in ["optionlists", "runscripts", "submitscripts"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let meta = format!(
            "[machine]\nnickname = \"{name}\"\n[queues.local]\ndefault = true\n\
             [variants.submitscript]\n\"default\" = [\"local\"]\n\
             [variants.runscript]\n\"default\" = [\"local\"]\n\
             [variants.optionlist]\nvariants = [\"default\"]\n{extra}"
        );
        fs::write(dir.join("meta.toml"), meta).unwrap();
        fs::write(dir.join("optionlists/default.toml"), "[options]\nVERSION = \"1\"\n").unwrap();
        fs::write(dir.join("runscripts/default.sh"), "#!/bin/sh\n").unwrap();
        fs::write(dir.join("submitscripts/default.sh"), "#!/bin/sh\n").unwrap();
    }

    fn overlay_error(e: &anyhow::Error) -> &OverlayGenerationError {
        e.downcast_ref::<OverlayGenerationError>()
            .unwrap_or_else(|| panic!("not a generation error: {e:#}"))
    }

    #[test]
    fn user_overlays_are_gated_on_their_generation() {
        let want = crate::build_info::MDB_GENERATION;
        let system = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let generation = |n: u32| format!("[cactup]\nmdb-generation = {n}\n");
        write_minimal_machine(user.path(), "current", &generation(want));
        write_minimal_machine(user.path(), "unmarked", "");
        // Older, and with a key this generation's closed schema rejects: the
        // generation is what gets reported, not the parse error.
        write_minimal_machine(user.path(), "older", "[cactup]\nmdb-generation = 0\nretired-key = 1\n");
        write_minimal_machine(user.path(), "newer", &generation(want + 1));
        // The system layer carries no per-machine generation and is not gated.
        write_minimal_machine(system.path(), "sys", &generation(0));
        let mdb = Mdb::with_roots(system.path().to_owned(), user.path().to_owned());

        assert_eq!(mdb.load("current").unwrap().meta.cactup.mdb_generation, Some(want));
        assert_eq!(mdb.load("unmarked").unwrap().meta.cactup.mdb_generation, None);
        mdb.load("sys").unwrap();

        let e = mdb.load("older").unwrap_err();
        let stale = overlay_error(&e);
        assert_eq!((stale.found, stale.want), (0, want));
        assert_eq!(stale.path, user.path().join("older/meta.toml"));
        // No GENERATIONS.md in this system root: the docs page is named.
        let text = e.to_string();
        assert!(text.contains("authors/mdb-generations.html"), "{text}");
        assert!(text.contains(&format!("mdb-generation = {want}")), "{text}");

        let e = mdb.load("newer").unwrap_err();
        assert_eq!(overlay_error(&e).found, want + 1);
        assert!(e.to_string().contains("cactup update"), "{e}");

        // A system MDB that ships GENERATIONS.md is pointed at locally, and
        // the machine-create base check applies the same gate.
        let dev = Mdb::with_roots(dev_mdb().system_root, user.path().to_owned());
        let e = dev.check_overlay("older", &user.path().join("older")).unwrap_err();
        assert_eq!(
            overlay_error(&e).guide,
            dev.system_root.join("GENERATIONS.md").display().to_string()
        );
        dev.check_overlay("unmarked", &user.path().join("unmarked")).unwrap();
    }

    #[test]
    fn a_generation_file_is_one_positive_integer() {
        assert_eq!(parse_generation("1\n"), Some(1));
        assert_eq!(parse_generation(" 12 "), Some(12));
        for bad in ["", "0", "-1", "one", "1 2"] {
            assert_eq!(parse_generation(bad), None, "{bad:?}");
        }
        assert_eq!(
            read_generation(&dev_mdb().system_root).unwrap(),
            Some(crate::build_info::MDB_GENERATION)
        );
    }

    #[test]
    fn an_mdb_path_override_must_match_the_generation() {
        let want = crate::build_info::MDB_GENERATION;
        let root = tempfile::tempdir().unwrap();
        let db = crate::database::Db::in_dir(root.path());
        // No GENERATION file (a fixture): accepted silently.
        assert_eq!(Mdb::open(Some(root.path()), &db).unwrap().system_root, root.path());
        fs::write(root.path().join("GENERATION"), format!("{want}\n")).unwrap();
        Mdb::open(Some(root.path()), &db).unwrap();
        // Generation 0 is not a generation, so "older" exists only past 1.
        let others = [Some((want + 1, true)), (want > 1).then(|| (want - 1, false))];
        for (other, hint) in others.into_iter().flatten() {
            fs::write(root.path().join("GENERATION"), format!("{other}\n")).unwrap();
            let err = Mdb::open(Some(root.path()), &db).err().unwrap().to_string();
            assert!(err.contains(&format!("generation {other}")), "{err}");
            assert_eq!(err.contains("cactup update"), hint, "{err}");
        }
        fs::write(root.path().join("GENERATION"), "garbage").unwrap();
        assert!(Mdb::open(Some(root.path()), &db).is_err());
    }
}
