//! `cactup use <alias>` — set the active installation.

use super::{machine, Ctx};
use crate::installation::Installation;
use crate::Res;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, alias: String) -> Res<()> {
    let path = ctx.db.update(|database| {
        let Some(entry) = database.installations.get(&alias) else {
            return Ok(None);
        };
        let path = entry.path.clone();
        database.active_installation = Some(alias.clone());
        Ok(Some(path))
    })?;

    let Some(path) = path else {
        println!("{}", format!("There is no installation named {}.", alias.bold()).bright_red());
        return Ok(());
    };
    println!("{}", format!("Switched to installation {}.", &alias.bold()).bright_green());

    // Backfill (§8.1, §11.5): an installation whose installation.toml predates
    // the sim-home/test-home keys gets them fixed now, from the machine's
    // [paths] — the hook the sim_home()/test_home() error messages point at.
    let inst = Installation::new(&alias, path);
    let meta = inst.meta()?;
    if meta.sim_home.is_none() || meta.test_home.is_none() {
        let machine = machine::resolve(ctx)?;
        inst.ensure_meta(&machine)?;
        println!("Recorded this installation's sim-home/test-home (from machine {}).", machine.name.bold());
    }
    Ok(())
}
