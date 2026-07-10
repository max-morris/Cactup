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
            let outcome = build::build(&installation, &machine, &args.name, &args.opts, false)?;
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
        ConfigCommand::Show { name } => show(&installation, name.as_deref()),
        ConfigCommand::Use { name } => {
            match ConfigMeta::load(&installation.cactus_root(), &name)? {
                None => bail!("no config named \"{name}\" in this installation (see `cactup config show`)"),
                // The two namespaces never interfere (§11.1).
                Some(meta) if meta.test => {
                    bail!("\"{name}\" is a test config; use `cactup test use {name}` (§11.1)")
                }
                Some(_) => {}
            }
            let locked = installation.locked()?;
            let mut meta = locked.meta()?;
            meta.active_config = Some(name.clone());
            locked.set_meta(&meta)?;
            println!("{}", format!("Config {} is now active.", name.bold()).bright_green());
            Ok(())
        }
        ConfigCommand::Delete { name, force } => delete(&installation, &name, force),
    }
}

/// The config names in this installation, oldest-built first, with metadata
/// when present. `test` filters to one §11.1 namespace: configs without
/// metadata (half-built) are grouped with the normal kind.
pub fn list_configs(cactus_root: &Path, test: bool) -> Res<Vec<(String, Option<ConfigMeta>)>> {
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
        if meta.as_ref().map(|m| m.test).unwrap_or(false) == test {
            out.push((name, meta));
        }
    }
    out.sort_by(|(an, a), (bn, b)| {
        let key = |m: &Option<ConfigMeta>| m.as_ref().and_then(|m| m.built);
        key(a).cmp(&key(b)).then_with(|| an.cmp(bn))
    });
    Ok(out)
}

fn show(installation: &Installation, name: Option<&str>) -> Res<()> {
    let cactus_root = installation.cactus_root();

    let Some(name) = name else {
        // Port of list-configurations (§7.1); test configs have their own
        // list under `cactup test show` (§11.1).
        let configs = list_configs(&cactus_root, false)?;
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
        return Ok(());
    };

    let Some(meta) = ConfigMeta::load(&cactus_root, name)? else {
        bail!("no config named \"{name}\" in this installation (see `cactup config show`)");
    };
    println!("{}", meta.name.bold());
    println!("  variant: {}", meta.variant);
    println!("  thornlist: {}", meta.thornlist);
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
    // The two namespaces never interfere (§11.1).
    if ConfigMeta::load(&cactus_root, name)?.is_some_and(|m| m.test) {
        bail!("\"{name}\" is a test config; use `cactup test delete {name}` (§11.1)");
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
        let remaining = list_configs(&cactus_root, false)?;
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

    // GC the executable cache under sim-home: dropping the config's exe may
    // have orphaned its CACHE/exe/<build-id> entry (§8.1). Best-effort — sims
    // built from it keep their own frozen (hard-linked) exe either way.
    if let Ok(inst_meta) = installation.meta() {
        if let Ok(sim_home) = inst_meta.sim_home() {
            let _ = cache::gc(sim_home, None);
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
            thornlist = "einsteintoolkit.th"
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
