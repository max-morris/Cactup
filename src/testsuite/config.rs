//! Test-config verbs (§11.4): `test build` / `show` / `use` / `delete` — the
//! test-namespace analogues of §7.1, operating on the `active-test-config`
//! pointer and filtered to `test = true` configs.

use crate::args::BuildOpts;
use crate::build::{self, ConfigMeta};
use crate::commands::config::list_configs;
use crate::commands::{machine, Ctx};
use crate::installation::Installation;
use crate::sim::cache;
use crate::Res;
use anyhow::bail;
use colored::Colorize;
use std::fs;

/// The default test-config name when `test build` is invoked bare (§11.3):
/// rebuild the active test-config when one exists, else start with this name.
const DEFAULT_TEST_CONFIG: &str = "tests";

pub fn build(ctx: &Ctx, name: Option<String>, opts: BuildOpts) -> Res<()> {
    let installation = Installation::resolve(ctx)?;
    let machine = machine::resolve(ctx)?;

    let name = match name {
        Some(n) => n,
        None => installation
            .meta()?
            .active_test_config
            .unwrap_or_else(|| DEFAULT_TEST_CONFIG.to_owned()),
    };

    // §11.4 point 3: the configs/ namespace is shared; a normal config owning
    // the name is a hard error (build() also guards, but with the generic
    // message — point at the right command here).
    if ConfigMeta::load(&installation.cactus_root(), &name)?.is_some_and(|m| !m.test) {
        bail!("\"{name}\" is a normal config; test configs need their own name (§11.1)");
    }

    let outcome = build::build(&installation, &machine, &name, &opts, true)?;

    // §11.4 point 4: a successful test build becomes the active test-config
    // (the normal active-config is untouched).
    let locked = installation.locked()?;
    let mut meta = locked.meta()?;
    if meta.active_test_config.as_deref() != Some(&name) {
        meta.active_test_config = Some(name.clone());
        locked.set_meta(&meta)?;
        println!("Test config {} is now the active test-config.", name.bold());
    }
    if outcome.rebuilt {
        println!(
            "{}",
            format!("Built test config {} (build-id {}).", name.bold(), outcome.meta.build_id)
                .bright_green()
        );
    }
    Ok(())
}

pub fn show(ctx: &Ctx, name: Option<&str>) -> Res<()> {
    let installation = Installation::resolve(ctx)?;
    let cactus_root = installation.cactus_root();

    let Some(name) = name else {
        let configs = list_configs(&cactus_root, true)?;
        if configs.is_empty() {
            println!("{}", "No test configs in this installation (see `cactup test build`).".bright_red());
            return Ok(());
        }
        let active = installation.meta()?.active_test_config;
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
        bail!("no test config named \"{name}\" (see `cactup test show`)");
    };
    if !meta.test {
        bail!("\"{name}\" is a normal config; see `cactup config show {name}` (§11.1)");
    }
    println!("{} {}", meta.name.bold(), "(test)".cyan());
    println!("  variant: {}", meta.variant);
    println!("  thornlist: {}", meta.thornlist);
    println!("  machine: {}", meta.machine);
    println!("  gpu: {}  compatible-queues: {}", meta.gpu, meta.compatible_queues.join(", "));
    if let Some(universe) = &meta.universe {
        println!("  universe: {universe} (coerce-run-universe: {})", meta.coerce_run_universe);
    }
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

pub fn use_cmd(ctx: &Ctx, name: &str) -> Res<()> {
    let installation = Installation::resolve(ctx)?;
    match ConfigMeta::load(&installation.cactus_root(), name)? {
        None => bail!("no test config named \"{name}\" (see `cactup test show`)"),
        Some(meta) if !meta.test => {
            bail!("\"{name}\" is a normal config; use `cactup config use {name}` (§11.1)")
        }
        Some(_) => {}
    }
    let locked = installation.locked()?;
    let mut meta = locked.meta()?;
    meta.active_test_config = Some(name.to_owned());
    locked.set_meta(&meta)?;
    println!("{}", format!("Test config {} is now active.", name.bold()).bright_green());
    Ok(())
}

/// `test delete` (§11.4): delete the test CONFIG (build + metadata) —
/// distinct from `test sim delete`, which deletes a test RUN.
pub fn delete(ctx: &Ctx, name: &str, force: bool) -> Res<()> {
    let installation = Installation::resolve(ctx)?;
    let cactus_root = installation.cactus_root();
    let config_dir = cactus_root.join("configs").join(name);
    if !config_dir.is_dir() {
        bail!("no test config named \"{name}\" in this installation");
    }
    match ConfigMeta::load(&cactus_root, name)? {
        Some(meta) if !meta.test => {
            bail!("\"{name}\" is a normal config; use `cactup config delete {name}` (§11.1)")
        }
        _ => {}
    }

    // §11.4: warn+refuse when registered test runs were built from it.
    let registry = installation.tests()?;
    let dependents: Vec<&str> = registry
        .tests
        .iter()
        .filter(|(_, e)| e.test_config == name)
        .map(|(n, _)| n.as_str())
        .collect();
    if !dependents.is_empty() && !force {
        bail!(
            "{} test run(s) reference test config \"{name}\": {} — pass -f to delete anyway",
            dependents.len(),
            dependents.join(", ")
        );
    }

    fs::remove_dir_all(&config_dir)?;
    let exe = build::executable_path(&cactus_root, name);
    if exe.exists() {
        fs::remove_file(&exe)?;
    }

    // Repoint the active test-config (§11.1: most-recently-built remaining
    // test config, else the null-test-config state).
    let locked = installation.locked()?;
    let mut meta = locked.meta()?;
    if meta.active_test_config.as_deref() == Some(name) {
        let remaining = list_configs(&cactus_root, true)?;
        meta.active_test_config = remaining
            .iter()
            .rev()
            .find(|(_, m)| m.is_some())
            .or(remaining.last())
            .map(|(n, _)| n.clone());
        locked.set_meta(&meta)?;
        match &meta.active_test_config {
            Some(next) => println!("Active test-config switched to {} (most recently built).", next.bold()),
            None => println!("The last test config is gone; the test namespace is now null-config."),
        }
    }
    drop(locked);

    // GC the executable cache under test-home if it became orphaned (§11.4).
    if let Ok(inst_meta) = installation.meta() {
        if let Ok(test_home) = inst_meta.test_home() {
            let _ = cache::gc(test_home, None);
        }
    }
    println!("{}", format!("Deleted test config {}.", name.bold()).bright_green());
    Ok(())
}
