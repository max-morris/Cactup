//! `cactup knob` — global default values (spec §5). Stored flat in the
//! global DB — a `~/.cactup` lives on exactly one machine, so knobs need no
//! machine keying; `user`/`email`/`mail-type` fall back to derived defaults
//! when unset.

use super::Ctx;
use crate::database::{knob_spec, KNOWN_KNOBS};
use crate::Res;
use anyhow::bail;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, name: Option<String>, value: Option<String>) -> Res<()> {
    let Some(name) = name else {
        println!("Knobs:");
        let db = ctx.db.read()?;
        for spec in KNOWN_KNOBS {
            let knob = spec.name;
            match (db.knob(knob), db.knob_or_default(knob)) {
                (Some(stored), _) => println!("  {knob} = {}", (spec.render)(stored)),
                (None, Some(derived)) => {
                    println!("  {knob} = {} {}", (spec.render)(&derived), "(derived)".dimmed())
                }
                (None, None) => println!("  {knob} {}", "(unset)".dimmed()),
            }
        }
        return Ok(());
    };

    let Some(spec) = knob_spec(&name) else {
        let known: Vec<&str> = KNOWN_KNOBS.iter().map(|s| s.name).collect();
        bail!("unknown knob \"{name}\" (known: {})", known.join(", "));
    };

    match value {
        None => {
            match ctx.db.read()?.knob_or_default(&name) {
                Some(stored) => println!("{}", (spec.render)(&stored)),
                None => println!("{}", "(unset)".dimmed()),
            }
        }
        Some(value) => {
            let stored = (spec.validate)(&value)?;
            ctx.db.update(|db| {
                db.set_knob(&name, stored.clone());
                Ok(())
            })?;
            let shown = (spec.render)(&stored);
            println!("{}", format!("Set {} = {}.", name.bold(), shown).bright_green());
        }
    }
    Ok(())
}
