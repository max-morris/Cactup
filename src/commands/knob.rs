//! `cactup knob` — global default values (spec §5). Stored flat in the
//! global DB — a `~/.cactup` lives on exactly one machine, so knobs need no
//! machine keying; `user`/`email`/`mail-type` fall back to derived defaults
//! when unset.

use super::Ctx;
use crate::database::KNOWN_KNOBS;
use crate::Res;
use anyhow::bail;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, name: Option<String>, value: Option<String>) -> Res<()> {
    let Some(name) = name else {
        println!("Knobs:");
        let db = ctx.db.read()?;
        for knob in KNOWN_KNOBS {
            match (db.knob(knob), db.knob_or_default(knob)) {
                (Some(stored), _) => println!("  {knob} = {stored}"),
                (None, Some(derived)) => println!("  {knob} = {derived} {}", "(derived)".dimmed()),
                (None, None) => println!("  {knob} {}", "(unset)".dimmed()),
            }
        }
        return Ok(());
    };

    if !KNOWN_KNOBS.contains(&name.as_str()) {
        bail!("unknown knob \"{name}\" (known: {})", KNOWN_KNOBS.join(", "));
    }

    match value {
        None => {
            match ctx.db.read()?.knob_or_default(&name) {
                Some(value) => println!("{value}"),
                None => println!("{}", "(unset)".dimmed()),
            }
        }
        Some(value) => {
            ctx.db.update(|db| {
                db.set_knob(&name, value.clone());
                Ok(())
            })?;
            println!("{}", format!("Set {} = {}.", name.bold(), value).bright_green());
        }
    }
    Ok(())
}
