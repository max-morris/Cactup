//! `cactup show` — show the active Einstein Toolkit installation (or a named
//! one) in detail. Listing every installation is `cactup list`.

use super::Ctx;
use crate::installation::Installation;
use crate::Res;
use anyhow::{anyhow, bail};
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, alias: Option<String>) -> Res<()> {
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
    match &entry.release {
        Some(release) => println!("  release:      {release}"),
        None => println!("  release:      (manual installation)"),
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
