//! `cactup knob` — machine-global default values (spec §5). Stored in the
//! global DB keyed by machine name; `user`/`email`/`mail-type` fall back to
//! derived defaults when unset.

use super::{machine, Ctx};
use crate::database::KNOWN_KNOBS;
use crate::Res;
use anyhow::bail;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, name: Option<String>, value: Option<String>) -> Res<()> {
    let machine = machine::resolve(ctx)?.name;

    let Some(name) = name else {
        println!("Knobs for machine {}:", machine.bold());
        let db = ctx.db.read()?;
        for knob in KNOWN_KNOBS {
            match (db.knob(&machine, knob), db.knob_or_default(&machine, knob)) {
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
            match ctx.db.read()?.knob_or_default(&machine, &name) {
                Some(value) => println!("{value}"),
                None => println!("{}", "(unset)".dimmed()),
            }
        }
        Some(value) => {
            ctx.db.update(|db| {
                db.set_knob(&machine, &name, value.clone());
                Ok(())
            })?;
            println!(
                "{}",
                format!("Set {} = {} for machine {}.", name.bold(), value, machine.bold()).bright_green()
            );
        }
    }
    Ok(())
}
