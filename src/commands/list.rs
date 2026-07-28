//! `cactup list` — list Einstein Toolkit installations on this machine.

use super::Ctx;
use crate::Res;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx) -> Res<()> {
    let database = ctx.db.read()?;

    if database.installations.is_empty() {
        println!("{}", "No installations found.".bright_red());
        return Ok(());
    }

    for installation in database.installations.values() {
        print!("- {}", installation.alias.bold());
        match (&installation.release, &installation.thornlist) {
            (Some(release), _) => print!(" (release {})", release.bold()),
            // Symmetric with the release case: name what it was built from.
            (None, Some(thornlist)) => print!(" (custom thornlist {})", thornlist.bold()),
            (None, None) => print!(" (custom)"),
        }
        if let Some(active_installation) = &database.active_installation && *active_installation == installation.alias {
            print!("{}", " (active)".bold().bright_green());
        }
        println!();
        if ctx.globals.verbose {
            println!("\t Path: {}", installation.path);
        }
    }

    Ok(())
}
