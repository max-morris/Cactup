//! `cactup use <alias>` — set the active installation.

use super::Ctx;
use crate::Res;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, alias: String) -> Res<()> {
    let switched = ctx.db.update(|database| {
        if !database.installations.contains_key(&alias) {
            return Ok(false);
        }
        database.active_installation = Some(alias.clone());
        Ok(true)
    })?;

    if switched {
        println!("{}", format!("Switched to installation {}.", &alias.bold()).bright_green());
    } else {
        println!("{}", format!("There is no installation named {}.", alias.bold()).bright_red());
    }
    Ok(())
}
