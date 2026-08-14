//! `cactup config` — the config subsystem (spec §7).

use super::{machine, Ctx};
use crate::args::ConfigCommand;
use crate::build::{self, ConfigMeta};
use crate::installation::Installation;
use crate::sim::cache;
use crate::Res;
use anyhow::bail;
use colored::Colorize;
use std::fs;
use std::path::Path;

pub fn dispatch(ctx: &Ctx, cmd: ConfigCommand) -> Res<()> {
    let installation = Installation::resolve(ctx)?;
    match cmd {
        ConfigCommand::Build(args) => {
            let machine = machine::resolve(ctx)?;
            let outcome = build::build(&installation, &machine, &args.name, &args.opts)?;
            // First build becomes active; later builds keep the pointer (§7.1).
            let locked = installation.locked()?;
            let mut meta = locked.meta()?;
            if meta.active_config.is_none() {
                meta.active_config = Some(args.name.clone());
                locked.set_meta(&meta)?;
                println!("Config {} is now the active config.", args.name.bold());
            }
            if outcome.rebuilt {
                println!(
                    "{}",
                    format!("Built config {} (build-id {}).", args.name.bold(), outcome.meta.build_id)
                        .bright_green()
                );
                // The rebuild minted a new build-id and replaced the exe, so
                // the previous build's CACHE/exe entry may now be
                // unreferenced (§8.1). Best-effort.
                if let Ok(inst_meta) = installation.meta() {
                    if let Ok(sim_home) = inst_meta.sim_home() {
                        let _ = cache::gc(sim_home, None);
                    }
                }
            }
            Ok(())
        }
        ConfigCommand::List => list(&installation),
        ConfigCommand::Show { name } => show(ctx, &installation, name.as_deref()),
        ConfigCommand::Use { name } => {
            if ConfigMeta::load(&installation.cactus_root(), &name)?.is_none() {
                bail!("no config named \"{name}\" in this installation (see `cactup config list`)");
            }
            let locked = installation.locked()?;
            let mut meta = locked.meta()?;
            meta.active_config = Some(name.clone());
            locked.set_meta(&meta)?;
            println!("{}", format!("Config {} is now active.", name.bold()).bright_green());
            Ok(())
        }
        ConfigCommand::Delete { name, force } => delete(&installation, &name, force),
        ConfigCommand::Delta { name } => {
            super::delta::config_delta(&installation, name, ctx.globals.verbose)
        }
    }
}

/// The config names in this installation, oldest-built first, with metadata
/// when present (half-built configs have none).
pub fn list_configs(cactus_root: &Path) -> Res<Vec<(String, Option<ConfigMeta>)>> {
    let configs_dir = cactus_root.join("configs");
    let mut out = Vec::new();
    let entries = match fs::read_dir(&configs_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        other => other?,
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = ConfigMeta::load(cactus_root, &name)?;
        out.push((name, meta));
    }
    out.sort_by(|(an, a), (bn, b)| {
        let key = |m: &Option<ConfigMeta>| m.as_ref().and_then(|m| m.built);
        key(a).cmp(&key(b)).then_with(|| an.cmp(bn))
    });
    Ok(out)
}

/// `cactup config list`: every config in the active installation (port of
/// list-configurations, §7.1).
fn list(installation: &Installation) -> Res<()> {
    let cactus_root = installation.cactus_root();
    let configs = list_configs(&cactus_root)?;
    if configs.is_empty() {
        println!("{}", "No configs in this installation.".bright_red());
        return Ok(());
    }
    let active = installation.meta()?.active_config;
    for (name, meta) in configs {
        print!("- {}", name.bold());
        if build::is_complete(&cactus_root, &name) {
            match meta.as_ref().and_then(|m| m.built) {
                Some(built) => print!(" [built {}]", built.format("%Y-%m-%d %H:%M")),
                None => print!(" [built]"),
            }
        } else {
            print!(" [incomplete]");
        }
        if active.as_deref() == Some(&name) {
            print!("{}", " (active)".bold().bright_green());
        }
        println!();
    }
    Ok(())
}

/// What the installation's live thornlist currently holds, from the DB
/// entry's provenance fields: an explicit-source refetch (which records
/// `current_*`) outranks install-time provenance, which is never rewritten.
fn live_thornlist_source(entry: &crate::database::CactusInstallation) -> String {
    let (release, from_list) = if entry.current_release.is_some() || entry.current_thornlist.is_some()
    {
        (entry.current_release.as_deref(), entry.current_thornlist.as_deref())
    } else {
        (entry.release.as_deref(), entry.thornlist.as_deref())
    };
    match (release, from_list) {
        (Some(release), _) => format!("release {release}"),
        (None, Some(list)) => format!("custom, from {list}"),
        (None, None) => "custom installation".to_owned(),
    }
}

/// `cactup config show [name]`: the active config, or a named one, in detail.
pub(crate) fn show(ctx: &Ctx, installation: &Installation, name: Option<&str>) -> Res<()> {
    let cactus_root = installation.cactus_root();

    // No name → the contextually-relevant config: the active one.
    let name = match name {
        Some(name) => name.to_owned(),
        None => installation.meta()?.active_config.ok_or_else(|| {
            anyhow::anyhow!(
                "this installation has no active config (null-config state); \
                 see `cactup config list`"
            )
        })?,
    };
    let name = name.as_str();

    let Some(meta) = ConfigMeta::load(&cactus_root, name)? else {
        bail!("no config named \"{name}\" in this installation (see `cactup config list`)");
    };
    println!("{}", meta.name.bold());
    println!("  variant: {}", meta.variant);
    // The bare path misleads on a custom installation: the live list's fixed
    // filename is `installation-default.th` whatever content it holds, so
    // the path alone can't say whether it's stock or custom. Say which
    // resolution rule produced the path and, for the live list, what it
    // actually holds.
    let provenance = if installation.is_live_thornlist(&meta.thornlist) {
        let source = ctx
            .db
            .read()
            .ok()
            .and_then(|db| db.installations.get(&installation.alias).map(live_thornlist_source));
        match source {
            Some(source) => format!("the installation's live list — {source}"),
            None => "the installation's live list".to_owned(),
        }
    } else if Path::new(&meta.thornlist).is_file() {
        "recorded from --thornlist".to_owned()
    } else {
        // resolve_thornlist rule 3: the recorded file is gone, so a rebuild
        // silently uses the config's snapshot — worth surfacing here.
        format!(
            "recorded from --thornlist; {}",
            "no longer readable — builds fall back to this config's snapshot".yellow()
        )
    };
    println!("  thornlist: {} ({provenance})", meta.thornlist);
    println!("  machine: {}", meta.machine);
    println!("  gpu: {}  compatible-queues: {}", meta.gpu, meta.compatible_queues.join(", "));
    if let Some(universe) = &meta.universe {
        println!("  universe: {universe} (coerce-run-universe: {})", meta.coerce_run_universe);
    }
    println!(
        "  flags: debug={} optimize={} unsafe={} profile={}",
        meta.flags.debug, meta.flags.optimize, meta.flags.unsafe_build, meta.flags.profile
    );
    println!("  config-id: {}", meta.config_id);
    println!("  build-id: {}", meta.build_id);
    if let Some(built) = meta.built {
        println!("  built: {built}");
    }
    println!(
        "  status: {}",
        if build::is_complete(&cactus_root, name) { "complete" } else { "incomplete" }
    );
    Ok(())
}

fn delete(installation: &Installation, name: &str, force: bool) -> Res<()> {
    let cactus_root = installation.cactus_root();
    let config_dir = cactus_root.join("configs").join(name);
    if !config_dir.is_dir() {
        bail!("no config named \"{name}\" in this installation");
    }
    // §7.1: warn+refuse when simulations were built from this config.
    let registry = installation.simulations()?;
    let dependents: Vec<&str> = registry
        .simulations
        .iter()
        .filter(|(_, e)| e.config == name)
        .map(|(n, _)| n.as_str())
        .collect();
    if !dependents.is_empty() && !force {
        bail!(
            "{} simulation(s) were built from config \"{name}\": {} — they keep working \
             (each holds its own frozen exe), but their config metadata will be gone. \
             Pass -f to delete anyway.",
            dependents.len(),
            dependents.join(", ")
        );
    }

    // §11: same for test runs — and unlike sims they hold no frozen exe, so
    // re-running their testsuite needs the config rebuilt.
    let tests = installation.tests()?;
    let test_dependents: Vec<&str> = tests
        .tests
        .iter()
        .filter(|(_, e)| e.config == name)
        .map(|(n, _)| n.as_str())
        .collect();
    if !test_dependents.is_empty() && !force {
        bail!(
            "{} test run(s) reference config \"{name}\": {} — their results stay readable, \
             but re-running them needs the config rebuilt. Pass -f to delete anyway.",
            test_dependents.len(),
            test_dependents.join(", ")
        );
    }

    fs::remove_dir_all(&config_dir)?;
    let exe = build::executable_path(&cactus_root, name);
    if exe.exists() {
        fs::remove_file(&exe)?;
    }

    // §7.1: deleting the active config repoints to the most-recently-built
    // remaining config; only zero configs left ⇒ null-config.
    let locked = installation.locked()?;
    let mut meta = locked.meta()?;
    if meta.active_config.as_deref() == Some(name) {
        let remaining = list_configs(&cactus_root)?;
        meta.active_config = remaining
            .iter()
            .rev() // most-recently-built last in the sorted list
            .find(|(_, m)| m.is_some())
            .or(remaining.last())
            .map(|(n, _)| n.clone());
        locked.set_meta(&meta)?;
        match &meta.active_config {
            Some(next) => println!("Active config switched to {} (most recently built).", next.bold()),
            None => println!("The last config is gone; this installation is now in the null-config state."),
        }
    }
    drop(locked);

    // GC the executable caches: dropping the config's exe may have orphaned
    // its CACHE/exe/<build-id> entry under sim-home (§8.1) or test-home
    // (§11.4). Best-effort — sims built from it keep their own frozen
    // (hard-linked) exe either way.
    if let Ok(inst_meta) = installation.meta() {
        if let Ok(sim_home) = inst_meta.sim_home() {
            let _ = cache::gc(sim_home, None);
        }
        if let Ok(test_home) = inst_meta.test_home() {
            let _ = cache::gc(test_home, None);
        }
    }
    println!("{}", format!("Deleted config {}.", name.bold()).bright_green());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn live_thornlist_source_prefers_refetch_provenance() {
        let entry = |release: Option<&str>,
                     thornlist: Option<&str>,
                     current_release: Option<&str>,
                     current_thornlist: Option<&str>| {
            crate::database::CactusInstallation {
                alias: "et".into(),
                release: release.map(str::to_owned),
                path: "/inst".into(),
                thornlist: thornlist.map(str::to_owned),
                current_release: current_release.map(str::to_owned),
                current_thornlist: current_thornlist.map(str::to_owned),
            }
        };
        // Install-time provenance, never refetched.
        assert_eq!(live_thornlist_source(&entry(Some("ET_2026_05"), None, None, None)), "release ET_2026_05");
        assert_eq!(
            live_thornlist_source(&entry(None, Some("/p/forks.th"), None, None)),
            "custom, from /p/forks.th"
        );
        assert_eq!(live_thornlist_source(&entry(None, None, None, None)), "custom installation");
        // An explicit-source refetch outranks install-time provenance — in
        // both directions (release install refetched to a custom list, and
        // custom install refetched to a release).
        assert_eq!(
            live_thornlist_source(&entry(Some("ET_2026_05"), None, None, Some("/p/forks.th"))),
            "custom, from /p/forks.th"
        );
        assert_eq!(
            live_thornlist_source(&entry(None, Some("/p/forks.th"), Some("ET_2026_11"), None)),
            "release ET_2026_11"
        );
    }

    #[test]
    fn delete_gcs_orphaned_cache_entry_only() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = Installation::new("et", tmp.path().join("et"));
        let sim_home = tmp.path().join("simhome");
        fs::create_dir_all(&sim_home).unwrap();
        {
            let locked = inst.locked().unwrap();
            let mut meta = locked.meta().unwrap();
            meta.sim_home = Some(sim_home.clone());
            locked.set_meta(&meta).unwrap();
        }

        let root = inst.cactus_root();
        let cfg_dir = root.join("configs").join("bbh");
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::write(
            cfg_dir.join("cactup-config.toml"),
            r#"
            name = "bbh"
            variant = "default"
            thornlist = "installation-default.th"
            machine = "fake"
            config-id = "config-bbh-1"
            build-id = "build-bbh-1"
            "#,
        )
        .unwrap();

        // The config's "built" exe, hard-linked into the sim-home cache (§8.1).
        let exe = build::executable_path(&root, "bbh");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"bbh binary").unwrap();
        let orphan = cache::ensure_cached(&sim_home, "build-bbh-1", &exe).unwrap();

        // A cache entry from an older build that a simulation still links.
        let old_src = tmp.path().join("old-binary");
        fs::write(&old_src, b"old binary").unwrap();
        let kept = cache::ensure_cached(&sim_home, "build-bbh-0", &old_src).unwrap();
        cache::link_into(&kept, &sim_home.join("sim-frozen-exe")).unwrap();
        fs::remove_file(&old_src).unwrap();

        delete(&inst, "bbh", false).unwrap();

        assert!(!root.join("configs/bbh").exists());
        assert!(!exe.exists());
        assert!(!orphan.exists(), "the deleted config's cache entry is reaped");
        assert!(kept.exists(), "a sim-referenced cache entry survives");
    }
}
