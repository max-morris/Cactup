//! `cactup machine` — machine resolution & user-MDB management (spec §4.3,
//! §4.7), plus the `resolve()` helper every machine-aware command goes
//! through.

use super::{prompt_with_default, Ctx};
use crate::args::MachineCommand;
use crate::database::{Db, DetectedMachine};
use crate::mdb::{discover, meta::ScriptKind, optionlist, Layer, Machine, Mdb, OverlayGenerationError};
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

pub fn dispatch(ctx: &Ctx, cmd: MachineCommand) -> Res<()> {
    // Opened per subcommand: `forget` only edits the DB and must not sync
    // the system MDB.
    let mdb = || Mdb::open(ctx.globals.mdb_path.as_deref(), &ctx.db);
    match cmd {
        MachineCommand::List => list(ctx, &mdb()?),
        MachineCommand::Show { name, variants } => show(ctx, &mdb()?, name, variants),
        MachineCommand::Create { name, from_existing, silent, no_discover } => create_machine(
            &ctx.db,
            &mdb()?,
            name,
            from_existing,
            silent,
            no_discover,
            ctx.globals.hostname.as_deref(),
        )
        .map(|_| ()),
        MachineCommand::Delete { name } => delete_machine(&ctx.db, &mdb()?, &name),
        MachineCommand::Forget => {
            let forgotten = ctx.db.update(|db| Ok(db.detected.take()))?;
            match forgotten {
                Some(cached) => println!("Forgot the cached detected machine ({}).", cached.name.bold()),
                None => println!("No detected machine was cached."),
            }
            Ok(())
        }
    }
}

/// Resolve the machine this command runs on (§4.3): `--machine` override →
/// the cached machine, trusted outright when its stamp names this host and
/// login session and re-verified against its own matcher otherwise →
/// discovery (prompting to disambiguate; `generic` on zero matches). Real
/// matches are cached; the zero-match fallback is not, so a later `machine
/// create` is picked up.
pub fn resolve(ctx: &Ctx) -> Res<Machine> {
    let mdb = Mdb::open(ctx.globals.mdb_path.as_deref(), &ctx.db)?;
    resolve_with(
        &ctx.db,
        &mdb,
        ctx.globals.machine.as_deref(),
        ctx.globals.hostname.as_deref(),
        ctx.globals.verbose,
    )
}

pub fn resolve_with(
    db: &Db,
    mdb: &Mdb,
    machine_flag: Option<&str>,
    hostname_override: Option<&str>,
    verbose: bool,
) -> Res<Machine> {
    match resolve_inner(db, mdb, machine_flag, hostname_override, verbose)? {
        Resolution::Known(machine) => Ok(machine),
        Resolution::Unrecognized(hostname) => {
            println!(
                "Unrecognized host {}; using the built-in {} machine. Run `{}` to persist a tuned local machine.",
                hostname.bold(),
                "generic".bold(),
                "cactup machine create".bold()
            );
            load_checked(mdb, "generic")
        }
    }
}

/// §4.7 install integration — the `setup-silent` successor. Resolves the
/// local machine like `resolve`, but an unrecognized host gets a user-MDB
/// machine created silently (and cached) instead of the in-place `generic`
/// fallback.
pub fn ensure_local_machine(ctx: &Ctx) -> Res<Machine> {
    let mdb = Mdb::open(ctx.globals.mdb_path.as_deref(), &ctx.db)?;
    ensure_local_machine_with(
        &ctx.db,
        &mdb,
        ctx.globals.machine.as_deref(),
        ctx.globals.hostname.as_deref(),
        ctx.globals.verbose,
    )
}

pub fn ensure_local_machine_with(
    db: &Db,
    mdb: &Mdb,
    machine_flag: Option<&str>,
    hostname_override: Option<&str>,
    verbose: bool,
) -> Res<Machine> {
    match resolve_inner(db, mdb, machine_flag, hostname_override, verbose)? {
        Resolution::Known(machine) => Ok(machine),
        Resolution::Unrecognized(hostname) => {
            println!(
                "Unrecognized host {}; persisting a tuned local machine in the user MDB.",
                hostname.bold()
            );
            let name = create_machine(db, mdb, None, None, true, false, hostname_override)?;
            load_checked(mdb, &name)
        }
    }
}

// Exactly one of these ever exists: `resolve_inner` returns it and `resolve`
// consumes it immediately. Boxing `Machine` to even the variants out would buy
// an allocation and a copy per call and save nothing.
#[allow(clippy::large_enum_variant)]
enum Resolution {
    Known(Machine),
    /// Zero discovery matches; carries the probed hostname.
    Unrecognized(String),
}

fn resolve_inner(
    db: &Db,
    mdb: &Mdb,
    machine_flag: Option<&str>,
    hostname_override: Option<&str>,
    verbose: bool,
) -> Res<Resolution> {
    if let Some(name) = machine_flag {
        return load_checked(mdb, name).map(Resolution::Known);
    }

    let hostname = discover::resolve_hostname(hostname_override);
    let session = session_key();
    let cached_failed = |name: &str, e: anyhow::Error| {
        e.context(format!(
            "the cached detected machine \"{name}\" failed to load; \
             `cactup machine forget` clears the cache, --machine overrides it"
        ))
    };

    // The cache: trusted as-is within the session that stamped it, re-checked
    // against the machine's own matcher the first time a new host or login
    // shell sees it (§4.3). Only a failed re-check reaches discovery.
    let mut previous = None;
    if let Some(cached) = db.read()?.detected {
        if cached.is_current(&hostname, session.as_deref()) {
            return load_checked(mdb, &cached.name)
                .map_err(|e| cached_failed(&cached.name, e))
                .map(Resolution::Known);
        }
        if mdb.verify(&cached.name, &hostname, verbose)? {
            if verbose {
                eprintln!("Machine {} still claims host {hostname}; keeping it.", cached.name.bold());
            }
            remember(db, &cached.name, &hostname, session.clone())?;
            return load_checked(mdb, &cached.name)
                .map_err(|e| cached_failed(&cached.name, e))
                .map(Resolution::Known);
        }
        println!(
            "{} the cached machine {} does not claim host {}; re-running discovery.",
            "note:".yellow().bold(),
            cached.name.bold(),
            hostname.bold()
        );
        previous = Some(cached.name);
    }

    let matches = mdb.discover(&hostname, verbose)?;
    let name = match matches.as_slice() {
        [] => match previous {
            // An interactive allocation puts the user on a compute-node
            // hostname that a login-only pattern never claims. That is not
            // a different machine, so the last positive answer stands —
            // silently from now on in this session — rather than dropping
            // to `generic` (§4.3).
            Some(previous) => {
                println!(
                    "{} no machine claims host {}; keeping {} (`cactup machine forget` re-detects, \
                     --machine overrides).",
                    "note:".yellow().bold(),
                    hostname.bold(),
                    previous.bold()
                );
                remember(db, &previous, &hostname, session)?;
                return load_checked(mdb, &previous).map_err(|e| cached_failed(&previous, e)).map(Resolution::Known);
            }
            None => return Ok(Resolution::Unrecognized(hostname)),
        },
        [only] => only.clone(),
        several => {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "host {hostname} is claimed by several machines ({}); \
                     pass --machine <name> to pick one",
                    several.join(", ")
                );
            }
            println!("Host {} is claimed by several machines:", hostname.bold());
            for name in several {
                println!("  - {name}");
            }
            loop {
                let choice = prompt_with_default("Which machine is this?", &several[0])?;
                if several.contains(&choice) {
                    break choice;
                }
                println!("{}", format!("{choice} is not one of the matches.").bright_red());
            }
        }
    };
    if let Some(previous) = &previous
        && *previous != name
    {
        println!("Detected machine changed from {} to {}.", previous.bold(), name.bold());
    }

    remember(db, &name, &hostname, session)?;
    load_checked(mdb, &name).map(Resolution::Known)
}

/// Cache `name` as the detected machine, stamped for `hostname` and the
/// current login session (§4.3).
fn remember(db: &Db, name: &str, hostname: &str, session: Option<String>) -> Res<()> {
    let record = DetectedMachine { name: name.to_owned(), hostname: hostname.to_owned(), session };
    db.update(|db| {
        db.detected = Some(record);
        Ok(())
    })
}

/// This login session's identity for the verification stamp: the session
/// leader's pid and start time, from /proc (never libc — D13). `None` when
/// /proc cannot say, which the stamp comparison treats as "never current".
fn session_key() -> Option<String> {
    let sid = crate::lock::proc_session(std::process::id())?;
    let start = crate::lock::proc_starttime(sid)?;
    Some(format!("{sid}:{start}"))
}

/// Load a machine and apply the §4.7 origin-staleness warning: an override
/// created with `--from-existing` warns when its system-MDB source has since
/// changed (or vanished) — the override does not pick up upstream changes.
fn load_checked(mdb: &Mdb, name: &str) -> Res<Machine> {
    let machine = mdb.load(name)?;
    if let Some(origin) = &machine.meta.cactup.origin {
        let source = mdb.system_root.join(&origin.from);
        if !source.join("meta.toml").is_file() {
            eprintln!(
                "{}",
                format!(
                    "Warning: {name} was created from \"{}\", which no longer exists in the system MDB.",
                    origin.from
                )
                .yellow()
            );
        } else if hash_machine_dir(&source)? != origin.hash {
            eprintln!(
                "{}",
                format!(
                    "Warning: the system-MDB machine \"{}\" has changed since {name} was created from it; \
                     {name} will NOT pick up the change (re-create it to refresh).",
                    origin.from
                )
                .yellow()
            );
        }
    }
    Ok(machine)
}

/// `cactup machine list`: every machine in the MDB.
fn list(ctx: &Ctx, mdb: &Mdb) -> Res<()> {
    let machines = mdb.machines()?;
    if machines.is_empty() {
        println!("{}", "No machines in the MDB.".bright_red());
        return Ok(());
    }
    let detected = ctx.db.read()?.detected.map(|d| d.name);
    for (name, layer) in machines {
        // One overlay of another MDB generation must not hide the rest of
        // the list: it is shown, annotated, instead of failing the command.
        let machine = match mdb.load(&name) {
            Ok(machine) => machine,
            Err(e) => match e.downcast_ref::<OverlayGenerationError>() {
                Some(stale) => {
                    println!(
                        "- {}{} {}",
                        name.bold(),
                        " (user MDB)".cyan(),
                        format!("[unusable: {}]", stale.brief()).yellow()
                    );
                    continue;
                }
                None => return Err(e),
            },
        };
        print!("- {}", name.bold());
        if let Some(nickname) = &machine.meta.machine.name {
            print!(" ({nickname})");
        }
        if let Some(status) = &machine.meta.machine.status {
            print!(" [{status}]");
        }
        if layer == Layer::User {
            print!("{}", " (user MDB)".cyan());
        }
        if detected.as_deref() == Some(&name) {
            print!("{}", " (detected)".bold().bright_green());
        }
        println!();
    }
    Ok(())
}

/// `cactup machine show [name]`: the machine this host resolves to (§4.3), or
/// a named one, in detail.
fn show(ctx: &Ctx, mdb: &Mdb, name: Option<String>, variants: bool) -> Res<()> {
    let machine = match name {
        Some(name) => load_checked(mdb, &name)?,
        None => resolve(ctx)?,
    };
    if variants {
        return show_variants(&machine);
    }
    print_summary(&machine)
}

/// Read-only machine section for the aggregate `cactup show` (§3): honors the
/// `--machine` override, otherwise uses only the **cached** `detected-machine`
/// — never discovery — so it cannot prompt, block on stdin, or write the DB.
pub(crate) fn show_cached_summary(ctx: &Ctx) -> Res<()> {
    let (name, verified_for) = match ctx.globals.machine.clone() {
        Some(name) => (Some(name), None),
        None => match ctx.db.read()?.detected {
            Some(cached) => (Some(cached.name), Some(cached.hostname)),
            None => (None, None),
        },
    };
    let Some(name) = name else {
        println!("{}", "(machine not yet detected — run `cactup machine show`)".yellow());
        return Ok(());
    };
    let mdb = Mdb::open(ctx.globals.mdb_path.as_deref(), &ctx.db)?;
    let machine = load_checked(&mdb, &name)?;
    print_summary(&machine)?;
    if let Some(hostname) = verified_for {
        println!("  detected for host: {hostname}");
    }
    Ok(())
}

/// The `machine show` summary block for an already-loaded machine.
fn print_summary(machine: &Machine) -> Res<()> {
    let meta = &machine.meta;
    println!("{} ({:?} MDB, {})", machine.name.bold(), machine.layer, machine.dir.display());
    for (label, value) in [
        ("name", &meta.machine.name),
        ("nickname", &meta.machine.nickname),
        ("hostname", &meta.machine.hostname),
        ("status", &meta.machine.status),
        ("location", &meta.machine.location),
        ("description", &meta.machine.description),
        ("webpage", &meta.machine.webpage),
    ] {
        if let Some(value) = value {
            println!("  {label}: {value}");
        }
    }
    if let Some(origin) = &meta.cactup.origin {
        println!("  created from: {} (hash {})", origin.from, origin.hash);
    }
    if let Some(generation) = meta.cactup.mdb_generation {
        println!("  mdb generation: {generation}");
    }
    println!(
        "  hardware: max-cpus-per-node={} default-cpus-per-task={} max-gpus-per-node={} \
         default-gpus-per-task={} threads-per-cpu={} memory={} MB",
        meta.hardware.max_cpus_per_node.map_or("?".into(), |v| v.to_string()),
        meta.hardware.default_cpus_per_task.map_or("?".into(), |v| v.to_string()),
        meta.hardware.max_gpus_per_node.map_or("?".into(), |v| v.to_string()),
        meta.hardware.default_gpus_per_task.map_or("?".into(), |v| v.to_string()),
        meta.hardware.threads_per_cpu(),
        meta.hardware.memory.map_or("?".into(), |v| v.to_string()),
    );
    // Display resolved paths when this host can resolve them (@USER@/@ENV()@
    // — §4.2); fall back to the raw meta.toml templates otherwise.
    let paths = meta.resolved_paths().unwrap_or_else(|_| meta.paths.clone());
    for (label, value) in [
        ("install-home", &paths.install_home),
        ("simulation-home", &paths.simulation_home),
        ("test-home", &paths.test_home),
        ("scratch-home", &paths.scratch_home),
    ] {
        if let Some(value) = value {
            println!("  {label}: {value}");
        }
    }
    println!("  queues:");
    for (name, queue) in &meta.queues {
        // Per-queue hardware overrides (§4.2) shown only where they differ
        // from the top-level [hardware] values.
        let mut overrides = String::new();
        for (label, value) in [
            ("max-cpus-per-node", queue.max_cpus_per_node.map(|v| v.to_string())),
            ("default-cpus-per-task", queue.default_cpus_per_task.map(|v| v.to_string())),
            ("max-gpus-per-node", queue.max_gpus_per_node.map(|v| v.to_string())),
            ("default-gpus-per-task", queue.default_gpus_per_task.map(|v| v.to_string())),
            ("threads-per-cpu", queue.threads_per_cpu.map(|v| v.to_string())),
            ("memory", queue.memory.map(|v| format!("{v} MB"))),
        ] {
            if let Some(value) = value {
                overrides.push_str(&format!("  {label} {value}"));
            }
        }
        println!(
            "    {name}{}{}{}{}{overrides}",
            if queue.default { " (default)" } else { "" },
            if queue.gpu { " [gpu]" } else { "" },
            queue.max_walltime.map_or(String::new(), |w| format!("  max-walltime {}", w.canonical())),
            // Build-universe compatibility list (§4.4), shown when present.
            queue
                .build_universes
                .as_ref()
                .map(|u| format!(" [build-universes: {}]", u.join(", ")))
                .unwrap_or_default(),
        );
    }
    for (label, kind) in [
        ("submitscripts", ScriptKind::Submit),
        ("runscripts", ScriptKind::Run),
        ("buildsubmitscripts", ScriptKind::BuildSubmit),
    ] {
        let sv = meta.script_variants(kind);
        // buildsubmitscript is empty on every machine until `build submit`
        // lands (Part B groundwork) — an empty section would just be noise.
        if sv.variants.is_empty() {
            continue;
        }
        let names: Vec<String> = sv
            .variants
            .iter()
            .map(|(n, e)| {
                format!(
                    "{n}{}{}{}",
                    if e.test { " [test]" } else { "" },
                    if e.default { " (default)" } else { "" },
                    // Build-universe compatibility list (§4.4), shown when present.
                    e.build_universes
                        .as_ref()
                        .map(|u| format!(" [build-universes: {}]", u.join(", ")))
                        .unwrap_or_default()
                )
            })
            .collect();
        println!("  {label}: {}", names.join(", "));
    }
    println!("  optionlists: {}", meta.variants.optionlist.variants.join(", "));
    if !meta.universes.is_empty() {
        println!("  universes: {}", meta.universes.keys().cloned().collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

/// `cactup machine show [name] --variants`: the machine's optionlist variants
/// with their `[cactup]` header details (description, compatible queues,
/// gpu/default markers, universe, per-variant thorn toggles).
fn show_variants(machine: &Machine) -> Res<()> {
    let listed = &machine.meta.variants.optionlist.variants;
    println!(
        "{} ({:?} MDB) — optionlist variants:",
        machine.name.bold(),
        machine.layer
    );
    if listed.is_empty() {
        println!("  {}", "(none)".bright_red());
        return Ok(());
    }
    for variant in listed {
        let path = machine.optionlist_path(variant);
        let header = optionlist::load_header(&path)
            .with_context(|| format!("failed to read optionlist header for variant \"{variant}\""))?;
        print!("- {}", variant.bold());
        if header.default {
            print!(" (default)");
        }
        if header.gpu {
            print!("{}", " [gpu]".cyan());
        }
        println!();
        if let Some(description) = &header.description {
            println!("    {description}");
        }
        let queues = if header.compatible_queues.is_empty() {
            "(any)".to_owned()
        } else {
            header.compatible_queues.join(", ")
        };
        println!("    compatible queues: {queues}");
        if let Some(universe) = &header.universe {
            println!("    build universe: {universe}");
        }
        if !header.enabled_thorns.is_empty() {
            println!("    enabled thorns: {}", header.enabled_thorns.join(", "));
        }
        if !header.disabled_thorns.is_empty() {
            println!("    disabled thorns: {}", header.disabled_thorns.join(", "));
        }
    }
    Ok(())
}

fn create_machine(
    db: &Db,
    mdb: &Mdb,
    name: Option<String>,
    from_existing: Option<Option<String>>,
    silent: bool,
    no_discover: bool,
    hostname_override: Option<&str>,
) -> Res<String> {
    let hostname = discover::resolve_hostname(hostname_override);
    let short = hostname.split('.').next().unwrap_or(&hostname).to_owned();
    let name = name.unwrap_or_else(|| short.clone());

    // The base to clone: generic, a named machine, or the detected one (§4.7).
    let base = match &from_existing {
        None => "generic".to_owned(),
        Some(Some(base)) => base.clone(),
        Some(None) => {
            let detected = db.read()?.detected.map(|d| d.name);
            match detected {
                Some(machine) => machine,
                None => {
                    let hits = mdb.discover(&hostname, false)?;
                    match hits.as_slice() {
                        [only] => only.clone(),
                        _ => bail!(
                            "--from-existing with no value clones the detected machine, \
                             but no machine is detected; name a base explicitly"
                        ),
                    }
                }
            }
        }
    };

    let (base_dir, base_layer) = mdb
        .machine_dir(&base)
        .ok_or_else(|| anyhow::anyhow!("base machine \"{base}\" does not exist in the MDB"))?;
    // A user-MDB base of another generation would be copied forward under a
    // generation stamp it does not deserve.
    if base_layer == Layer::User {
        mdb.check_overlay(&base, &base_dir)?;
    }
    let target = mdb.user_root.join(&name);
    if target.exists() {
        bail!(
            "machine \"{name}\" already exists in the user MDB ({}); delete it first",
            target.display()
        );
    }

    copy_dir(&base_dir, &target)?;
    // The base's matchers must not come along: a clone of `mike` is not
    // claimed by `^mike\d+`. The new machine gets its own below (or none).
    for matcher in ["discover.py", "hostname.regexp"] {
        match fs::remove_file(target.join(matcher)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to remove {}", target.join(matcher).display()));
            }
        }
    }

    // Patch the copied meta.toml: identity, concrete autodetected hardware
    // (§4.6 done once, at create time), homes, and origin provenance (D2).
    // Round-tripping through toml::Table drops the base's comments — accepted.
    let meta_path = target.join("meta.toml");
    let mut table: toml::Table = fs::read_to_string(&meta_path)?
        .parse()
        .with_context(|| format!("Failed to parse {}", meta_path.display()))?;

    let machine_tbl = subtable(&mut table, "machine");
    machine_tbl.insert("name".into(), name.clone().into());
    machine_tbl.insert("nickname".into(), short.clone().into());
    machine_tbl.insert("hostname".into(), hostname.clone().into());
    machine_tbl.insert("status".into(), "personal".into());

    let hw = crate::mdb::autodetect::detect();
    let hardware_tbl = subtable(&mut table, "hardware");
    hardware_tbl.remove("autodetect");
    hardware_tbl.insert("max-cpus-per-node".into(), (hw.cores as i64).into());
    if let Some(memory) = hw.memory_mb {
        hardware_tbl.insert("memory".into(), (memory as i64).into());
    }
    // Only written when GPUs were actually found: the key's absence is a valid
    // state (§4.2), so a GPU-less host must not record a hard `0`.
    if let Some(gpus) = hw.gpus {
        hardware_tbl.insert("max-gpus-per-node".into(), (gpus as i64).into());
    }

    let default_sim_home = crate::CACTUP_ROOT.join("simulations");
    let default_install_home = crate::CACTUP_ROOT.join("cacti");
    let (sim_home, install_home) = if silent {
        (default_sim_home.display().to_string(), default_install_home.display().to_string())
    } else {
        (
            prompt_with_default("Where should simulation output live?", &default_sim_home.display().to_string())?,
            prompt_with_default("Where should installations live?", &default_install_home.display().to_string())?,
        )
    };
    let paths_tbl = subtable(&mut table, "paths");
    paths_tbl.insert("simulation-home".into(), sim_home.into());
    paths_tbl.insert("install-home".into(), install_home.into());

    // The generation this overlay is written for, checked at every load.
    subtable(&mut table, "cactup")
        .insert("mdb-generation".into(), i64::from(crate::build_info::MDB_GENERATION).into());

    if from_existing.is_some() {
        let hash = hash_machine_dir(&base_dir)?;
        let cactup_tbl = subtable(&mut table, "cactup");
        let mut origin = toml::Table::new();
        origin.insert("from".into(), base.clone().into());
        origin.insert("hash".into(), hash.into());
        cactup_tbl.insert("origin".into(), origin.into());
    }

    fs::write(&meta_path, toml::to_string_pretty(&table)?)
        .with_context(|| format!("Failed to write {}", meta_path.display()))?;

    // Selectable only via --machine when --no-discover: no matcher file at
    // all. Otherwise a hostname.regexp claiming this host's FQDN or short
    // name (the regexp is tested against both forms — §4.3), and the new
    // machine becomes the detected one for this host and session.
    if !no_discover {
        let regexp_path = target.join("hostname.regexp");
        fs::write(&regexp_path, format!("^(?:{}|{})$\n", regex::escape(&hostname), regex::escape(&short)))
            .with_context(|| format!("Failed to write {}", regexp_path.display()))?;
        remember(db, &name, &hostname, session_key())?;
    }

    // The user/email/allocation knobs (§4.7, §5) — global, since this
    // `~/.cactup` lives on the machine being created. Only knobs never set
    // before are prompted for (silent takes the derived defaults).
    let snapshot = db.read()?;
    let mut answers = Vec::new();
    for (knob, question) in [
        ("user", "Your username on this machine?"),
        ("email", "Your email (for job notifications)?"),
        ("allocation", "Your default allocation/account (empty for none)?"),
    ] {
        if snapshot.knob(knob).is_none() {
            let default = snapshot.knob_or_default(knob).unwrap_or_default();
            let answer = if silent { default } else { prompt_with_default(question, &default)? };
            if !answer.is_empty() {
                answers.push((knob, answer));
            }
        }
    }
    if !answers.is_empty() {
        db.update(|db| {
            for (knob, value) in answers {
                db.set_knob(knob, value);
            }
            Ok(())
        })?;
    }

    // Prove the new machine loads before declaring success.
    mdb.load(&name)?;
    println!(
        "{}",
        format!("Created machine {} in the user MDB ({}).", name.bold(), target.display()).bright_green()
    );
    Ok(name)
}

fn delete_machine(db: &Db, mdb: &Mdb, name: &str) -> Res<()> {
    match mdb.machine_dir(name) {
        None => bail!("no machine named \"{name}\" in the MDB"),
        Some((_, Layer::System)) => bail!(
            "\"{name}\" is a system-MDB machine and cannot be deleted; \
             override it instead with `cactup machine create {name} --from-existing {name}`"
        ),
        Some((dir, Layer::User)) => {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("Failed to remove {}", dir.display()))?;
            db.update(|db| {
                if db.detected.as_ref().is_some_and(|d| d.name == name) {
                    db.detected = None;
                }
                Ok(())
            })?;
            println!("{}", format!("Deleted user-MDB machine {}.", name.bold()).bright_green());
            Ok(())
        }
    }
}

fn subtable<'t>(table: &'t mut toml::Table, key: &str) -> &'t mut toml::Table {
    table
        .entry(key)
        .or_insert_with(|| toml::Table::new().into())
        .as_table_mut()
        .expect("meta.toml top-level entries are tables")
}

fn copy_dir(from: &Path, to: &Path) -> Res<()> {
    fs::create_dir_all(to).with_context(|| format!("Failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("Failed to list {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy() == "__pycache__" {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("Failed to copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Deterministic content hash of a machine directory (relative path + bytes
/// of every file, FNV-1a 64) — the §4.7 origin-staleness fingerprint.
pub fn hash_machine_dir(dir: &Path) -> Res<String> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort();

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    };
    for rel in files {
        feed(rel.as_bytes());
        feed(&[0]);
        feed(&fs::read(dir.join(&rel)).with_context(|| format!("Failed to read {rel} under {}", dir.display()))?);
        feed(&[0]);
    }
    Ok(format!("{hash:016x}"))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Res<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("Failed to list {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let lossy = name.to_string_lossy();
        if lossy == "__pycache__" || lossy.ends_with(".pyc") {
            continue; // caches must not perturb the fingerprint
        }
        if entry.file_type()?.is_dir() {
            collect_files(root, &entry.path(), out)?;
        } else {
            out.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("child of root")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dev_system_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb")
    }

    struct Sandbox {
        _user: tempfile::TempDir,
        _dbdir: tempfile::TempDir,
        mdb: Mdb,
        db: Db,
    }

    fn sandbox() -> Sandbox {
        let user = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let mdb = Mdb::with_roots(dev_system_root(), user.path().to_owned());
        let db = Db::in_dir(dbdir.path());
        Sandbox { mdb, db, _user: user, _dbdir: dbdir }
    }

    #[test]
    fn hash_is_deterministic_and_content_sensitive() {
        let a = hash_machine_dir(&dev_system_root().join("mel5")).unwrap();
        let b = hash_machine_dir(&dev_system_root().join("mel5")).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, hash_machine_dir(&dev_system_root().join("generic")).unwrap());
    }

    fn cached(sb: &Sandbox) -> Option<DetectedMachine> {
        sb.db.read().unwrap().detected
    }

    fn cached_name(sb: &Sandbox) -> Option<String> {
        cached(sb).map(|d| d.name)
    }

    #[test]
    fn resolve_uses_flag_then_cache_then_discovery() {
        let sb = sandbox();

        // --machine wins outright and does not touch the cache.
        let m = resolve_with(&sb.db, &sb.mdb, Some("mel5"), None, false).unwrap();
        assert_eq!(m.name, "mel5");
        assert_eq!(cached(&sb), None);

        // Discovery match is cached, stamped for this host and session (§4.3).
        let m = resolve_with(&sb.db, &sb.mdb, None, Some("melete05.cct.lsu.edu"), false).unwrap();
        assert_eq!(m.name, "mel5");
        let stamp = cached(&sb).unwrap();
        assert_eq!(stamp.name, "mel5");
        assert_eq!(stamp.hostname, "melete05.cct.lsu.edu");
        assert_eq!(stamp.session, session_key());

        // A new hostname re-verifies the cached machine against its own
        // matcher: the short name still claims mel5, so only the stamp moves.
        let m = resolve_with(&sb.db, &sb.mdb, None, Some("melete05"), false).unwrap();
        assert_eq!(m.name, "mel5");
        assert_eq!(cached(&sb).unwrap().hostname, "melete05");

        // A host nobody claims keeps the last positive answer (an interactive
        // allocation is not a different machine), restamped so it stays quiet.
        let m = resolve_with(&sb.db, &sb.mdb, None, Some("c123.compute.internal"), false).unwrap();
        assert_eq!(m.name, "mel5");
        assert_eq!(cached(&sb).unwrap().hostname, "c123.compute.internal");

        // A host another machine claims replaces it.
        let m = resolve_with(&sb.db, &sb.mdb, None, Some("mike2.hpc.lsu.edu"), false).unwrap();
        assert_eq!(m.name, "mike");
        assert_eq!(cached_name(&sb).as_deref(), Some("mike"));
    }

    #[test]
    fn cache_hit_needs_same_hostname_and_session() {
        let sb = sandbox();
        // A user machine whose matcher is then removed: only a genuine
        // stamp hit can resolve it without discovery.
        create_machine(
            &sb.db,
            &sb.mdb,
            Some("mybox".into()),
            None,
            true,
            false,
            Some("mybox.example.org"),
        )
        .unwrap();
        let dir = sb.mdb.machine_dir("mybox").unwrap().0;
        fs::remove_file(dir.join("hostname.regexp")).unwrap();

        if session_key().is_some() {
            // Same host, same session: trusted as-is.
            let m = resolve_with(&sb.db, &sb.mdb, None, Some("mybox.example.org"), false).unwrap();
            assert_eq!(m.name, "mybox");
            // A stamp from another session is never current.
            remember(&sb.db, "mybox", "mybox.example.org", Some("1:1".into())).unwrap();
        }
        // Verification fails (no matcher), discovery finds nobody: kept.
        let m = resolve_with(&sb.db, &sb.mdb, None, Some("mybox.example.org"), false).unwrap();
        assert_eq!(m.name, "mybox");
        assert_eq!(cached(&sb).unwrap().session, session_key());
        // A stale stamp for an unknown session is re-verified, not trusted.
        remember(&sb.db, "mybox", "mybox.example.org", None).unwrap();
        let m = resolve_with(&sb.db, &sb.mdb, None, Some("melete05.cct.lsu.edu"), false).unwrap();
        assert_eq!(m.name, "mel5");
    }

    #[test]
    fn zero_match_falls_back_to_generic_uncached() {
        let sb = sandbox();
        let m = resolve_with(&sb.db, &sb.mdb, None, Some("nobodys.laptop"), false).unwrap();
        assert_eq!(m.name, "generic");
        // NOT cached, so a later `machine create` gets discovered.
        assert_eq!(cached(&sb), None);
    }

    #[test]
    fn create_silent_and_delete() {
        let sb = sandbox();

        create_machine(
            &sb.db,
            &sb.mdb,
            Some("mybox".into()),
            Some(Some("mel5".into())), // --from-existing mel5
            true,                      // --silent
            false,
            Some("mybox.example.org"),
        )
        .unwrap();

        let machine = sb.mdb.load("mybox").unwrap();
        assert_eq!(machine.layer, Layer::User);
        // Concrete autodetected hardware was persisted; autodetect flag gone.
        assert!(!machine.meta.hardware.autodetect);
        assert!(machine.meta.hardware.max_cpus_per_node.unwrap() >= 1);
        // Origin provenance recorded with the base's current hash.
        let origin = machine.meta.cactup.origin.as_ref().unwrap();
        assert_eq!(origin.from, "mel5");
        assert_eq!(origin.hash, hash_machine_dir(&dev_system_root().join("mel5")).unwrap());
        // Homes written explicitly (silent defaults).
        assert!(machine.meta.paths.simulation_home.as_deref().unwrap().ends_with("simulations"));
        // Stamped with the MDB generation it was written for.
        assert_eq!(machine.meta.cactup.mdb_generation, Some(crate::build_info::MDB_GENERATION));
        // The cache now points at the new machine, stamped for its host.
        let stamp = cached(&sb).unwrap();
        assert_eq!((stamp.name.as_str(), stamp.hostname.as_str()), ("mybox", "mybox.example.org"));

        // The generated hostname.regexp claims the FQDN and the short name —
        // and the base's matcher did not come along, so mel5's host is not
        // claimed by the clone.
        assert!(sb.mdb.verify("mybox", "mybox.example.org", false).unwrap());
        assert!(sb.mdb.verify("mybox", "mybox", false).unwrap());
        assert!(!sb.mdb.verify("mybox", "elsewhere.org", false).unwrap());
        assert!(!sb.mdb.verify("mybox", "melete05.cct.lsu.edu", false).unwrap());
        assert_eq!(sb.mdb.discover("melete05.cct.lsu.edu", false).unwrap(), ["mel5"]);

        // Deleting a system machine is refused; a user machine works and
        // clears the cache.
        assert!(delete_machine(&sb.db, &sb.mdb, "mel5").is_err());
        delete_machine(&sb.db, &sb.mdb, "mybox").unwrap();
        assert!(sb.mdb.machine_dir("mybox").is_none());
        assert_eq!(cached(&sb), None);
    }

    #[test]
    fn ensure_local_machine_creates_on_unknown_host_only() {
        // Unknown host: a user-MDB machine is persisted silently and cached
        // (the setup-silent successor, §4.7).
        let sb = sandbox();
        let m = ensure_local_machine_with(&sb.db, &sb.mdb, None, Some("newlaptop.example.org"), false)
            .unwrap();
        assert_eq!(m.name, "newlaptop");
        assert_eq!(m.layer, Layer::User);
        assert_eq!(cached_name(&sb).as_deref(), Some("newlaptop"));
        // The generated hostname.regexp claims it, so a resolve from another
        // session (a stale stamp) re-verifies and finds it too.
        remember(&sb.db, "newlaptop", "newlaptop.example.org", None).unwrap();
        let m = resolve_with(&sb.db, &sb.mdb, None, Some("newlaptop.example.org"), false).unwrap();
        assert_eq!(m.name, "newlaptop");

        // Known host: resolves the existing machine, creates nothing.
        let sb = sandbox();
        let m = ensure_local_machine_with(&sb.db, &sb.mdb, None, Some("melete05.cct.lsu.edu"), false)
            .unwrap();
        assert_eq!(m.name, "mel5");
        assert!(sb.mdb.machine_dir("mel5").is_some_and(|(_, layer)| layer == Layer::System));
    }

    #[test]
    fn create_no_discover_never_matches_and_does_not_cache() {
        let sb = sandbox();
        create_machine(&sb.db, &sb.mdb, Some("ghost".into()), None, true, true, Some("ghost.example"))
            .unwrap();
        assert_eq!(cached(&sb), None);
        let machine = sb.mdb.load("ghost").unwrap();
        assert!(machine.meta.cactup.origin.is_none(), "plain create records no origin");
        assert!(!machine.dir.join("hostname.regexp").exists());
        assert!(!machine.dir.join("discover.py").exists());
        assert!(!sb.mdb.verify("ghost", "ghost.example", false).unwrap());
        assert!(sb.mdb.discover("ghost.example", false).unwrap().is_empty());
    }

    #[test]
    fn overlays_of_another_generation_are_refused_as_bases_and_annotated_in_lists() {
        let sb = sandbox();
        create_machine(&sb.db, &sb.mdb, Some("old".into()), None, true, true, Some("old.example"))
            .unwrap();
        let meta_path = sb.mdb.user_root.join("old/meta.toml");
        let text = fs::read_to_string(&meta_path).unwrap();
        let stamp = format!("mdb-generation = {}", crate::build_info::MDB_GENERATION);
        assert!(text.contains(&stamp), "create writes the generation:\n{text}");
        fs::write(&meta_path, text.replace(&stamp, "mdb-generation = 0")).unwrap();

        // A user-MDB base of an older generation cannot be cloned forward.
        let e = create_machine(
            &sb.db,
            &sb.mdb,
            Some("copy".into()),
            Some(Some("old".into())),
            true,
            true,
            Some("copy.example"),
        )
        .unwrap_err();
        assert!(e.downcast_ref::<OverlayGenerationError>().is_some(), "{e:#}");
        assert!(sb.mdb.machine_dir("copy").is_none());

        // The list shows it, annotated, rather than failing as a whole.
        let ctx = Ctx {
            globals: crate::args::GlobalOpts {
                verbose: false,
                trace: false,
                manifest_url: String::new(),
                mdb_path: None,
                machine: None,
                installation: None,
                hostname: None,
                knob: Vec::new(),
            },
            db: Db::in_dir(sb._dbdir.path()),
        };
        list(&ctx, &sb.mdb).unwrap();
        assert!(sb.mdb.load("old").is_err());
    }

    #[test]
    fn forget_clears_the_record() {
        let sb = sandbox();
        remember(&sb.db, "mel5", "melete05", session_key()).unwrap();
        let forgotten = sb.db.update(|db| Ok(db.detected.take())).unwrap();
        assert_eq!(forgotten.unwrap().name, "mel5");
        assert_eq!(cached(&sb), None);
    }
}
