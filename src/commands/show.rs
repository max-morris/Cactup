//! `cactup show` — list Einstein Toolkit installations on this machine.

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
        if let Some(release) = &installation.release {
            print!(" (release {})", release.bold());
        } else {
            print!(" (manual installation)");
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
