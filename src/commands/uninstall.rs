//! `cactup uninstall <alias>` — remove an installation (spec §3.1).

use super::{prompt_with_default, Ctx};
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::fs;
use std::path::PathBuf;

pub fn dispatch(ctx: &Ctx, alias: String, force: bool) -> Res<()> {
    let db = ctx.db.read()?;
    let Some(entry) = db.installations.get(&alias) else {
        bail!("no installation named \"{alias}\" (see `cactup show`)");
    };
    let path = PathBuf::from(&entry.path);

    if !force {
        let answer = prompt_with_default(
            &format!(
                "Really delete installation {} and its directory {}? Simulation/test output \
                 under its sim-home/test-home is left untouched. Type \"yes\" to confirm",
                alias.bold(),
                path.display()
            ),
            "no",
        )?;
        if !answer.eq_ignore_ascii_case("yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    if path.is_dir() {
        fs::remove_dir_all(&path)
            .with_context(|| format!("Failed to remove {}", path.display()))?;
    } else {
        println!(
            "{} the installation directory {} was already gone; removing the registration only",
            "note:".yellow(),
            path.display()
        );
    }

    // Field-scoped RMW (§2.3): drop the entry and, if it was active, the
    // active pointer — never silently promote another installation.
    let was_active = ctx.db.update(|database| {
        database.installations.shift_remove(&alias);
        Ok(if database.active_installation.as_deref() == Some(&alias) {
            database.active_installation = None;
            true
        } else {
            false
        })
    })?;

    println!("{}", format!("Uninstalled {}.", alias.bold()).bright_green());
    if was_active {
        println!(
            "It was the active installation; pick a new one with `{}`.",
            "cactup use <alias>".bold()
        );
    }
    Ok(())
}
