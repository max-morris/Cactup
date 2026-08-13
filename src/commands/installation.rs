//! `cactup installation` — the installation subsystem (spec §3): list, show,
//! use, and refetch Einstein Toolkit installations. `cactup list`/`show`/`use`
//! at the top level are shorthand for `list`/`show`/`use` here.

use super::Ctx;
use crate::args::InstallationCommand;
use crate::installation::Installation;
use crate::Res;
use anyhow::{anyhow, bail};
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, cmd: InstallationCommand) -> Res<()> {
    match cmd {
        InstallationCommand::List => super::list::dispatch(ctx),
        InstallationCommand::Show { alias } => show_installation(ctx, alias),
        InstallationCommand::Use { alias } => super::use_cmd::dispatch(ctx, alias),
        InstallationCommand::Refetch(args) => super::refetch::dispatch(ctx, args),
        InstallationCommand::Delta { alias } => super::delta::installation_delta(ctx, alias),
    }
}

/// `cactup installation show [alias]`: the active installation, or a named
/// one, in detail. Listing every installation is `cactup installation list`.
pub(crate) fn show_installation(ctx: &Ctx, alias: Option<String>) -> Res<()> {
    let database = ctx.db.read()?;

    // No alias → the contextually-relevant installation: the active one.
    let alias = match alias {
        Some(alias) => alias,
        None => database.active_installation.clone().ok_or_else(|| {
            anyhow!(
                "no active installation; run `cactup install`, or `cactup use <alias>` \
                 to activate an existing one (see `cactup list`)"
            )
        })?,
    };

    let Some(entry) = database.installations.get(&alias) else {
        bail!("no installation named \"{alias}\" (see `cactup list`)");
    };
    let active = database.active_installation.as_deref() == Some(&alias);

    print!("{}", entry.alias.bold());
    if active {
        print!("{}", " (active)".bold().bright_green());
    }
    println!();
    match (&entry.release, &entry.thornlist) {
        (Some(release), _) => println!("  release:      {release}"),
        // A custom installation is identified by the thornlist it was built
        // from — that is the only thing distinguishing it from any other.
        (None, Some(thornlist)) => {
            println!("  release:      (custom installation from {thornlist})")
        }
        (None, None) => println!("  release:      (custom installation)"),
    }
    // Install-time provenance above; where a refetch has since taken the
    // tree below (§2.1 current-release / current-thornlist). Shown only
    // when it actually differs.
    match (&entry.current_release, &entry.current_thornlist) {
        (Some(current), _) if Some(current) != entry.release.as_ref() => {
            println!("  now on:       {current} (refetched)")
        }
        (None, Some(current)) if Some(current) != entry.thornlist.as_ref() => {
            println!("  now on:       thornlist {current} (refetched)")
        }
        _ => {}
    }
    println!("  path:         {}", entry.path);

    let inst = Installation::new(entry.alias.clone(), &entry.path);
    if let Ok(meta) = inst.meta() {
        match &meta.active_config {
            Some(config) => println!("  active-config: {config}"),
            None => println!("  active-config: {}", "(null-config)".yellow()),
        }
        if let Some(sim_home) = &meta.sim_home {
            println!("  sim-home:     {}", sim_home.display());
        }
        if let Some(test_home) = &meta.test_home {
            println!("  test-home:    {}", test_home.display());
        }
    }
    if let Ok(sims) = inst.simulations() {
        println!("  simulations:  {} (see `cactup sim list`)", sims.simulations.len());
    }
    if let Ok(tests) = inst.tests() {
        println!("  test runs:    {} (see `cactup test list`)", tests.tests.len());
    }

    Ok(())
}
