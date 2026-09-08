//! `cactup knob` — global default values (spec §5). Stored flat in the
//! global DB — a `~/.cactup` lives on exactly one machine, so knobs need no
//! machine keying; `user`/`email`/`mail-type` fall back to derived defaults
//! when unset.
//!
//! Two kinds share the one map: **standard** knobs (`KNOWN_KNOBS`, each with
//! a `KnobSpec`) and **custom** knobs — user-named, free-form values that
//! exist to be read by `@KNOB(name)@` in parfiles, scripts and optionlists
//! (§6.1). A custom knob is created with `-c/--custom` and removed with
//! `knob delete` (which merely *unsets* a standard knob — those always
//! exist); setting a non-standard name that was never created is an error,
//! so a typo cannot quietly mint a knob nothing reads. The `-K` overlay
//! (§5.1) shows through every read here and is marked as such.

use super::Ctx;
use crate::args::KnobCommand;
use crate::database::{knob_display_form, knob_overrides, knob_spec, validate_knob_name, KNOWN_KNOBS};
use crate::Res;
use anyhow::bail;
use colored::Colorize;

/// The `knob <subcommand>` forms (§5).
pub fn dispatch_sub(ctx: &Ctx, cmd: KnobCommand) -> Res<()> {
    match cmd {
        KnobCommand::Delete { name } => delete(ctx, &name),
    }
}

/// `knob delete <name>`. A standard knob always exists, so it is *unset*: the
/// stored value goes and the derived/built-in default applies again. A
/// custom knob is removed outright (creating it again needs `-c`). A name
/// that is neither is an error rather than a silent no-op.
fn delete(ctx: &Ctx, name: &str) -> Res<()> {
    let standard = knob_spec(name).is_some();
    if !standard {
        validate_knob_name(name)?;
    }
    ctx.db.update(|db| {
        if db.knobs.shift_remove(name).is_none() && !standard {
            bail!("{}", unknown_custom(name));
        }
        Ok(())
    })?;
    let message = if standard {
        format!("Unset {}; it falls back to its default.", name.bold())
    } else {
        format!("Deleted custom knob {}.", name.bold())
    };
    println!("{}", message.bright_green());
    Ok(())
}

pub fn dispatch(ctx: &Ctx, name: Option<String>, value: Option<String>, custom: bool) -> Res<()> {
    let Some(name) = name else {
        if custom {
            bail!("-c/--custom creates a knob: name the knob and give it a value");
        }
        return print_all(ctx);
    };

    // Standard knobs are always addressable; anything else must be a valid
    // custom knob name.
    let standard = knob_spec(&name).is_some();
    if !standard {
        validate_knob_name(&name)?;
    }

    match value {
        None => {
            if custom {
                bail!("-c/--custom creates a knob: give \"{name}\" a value to set");
            }
            let db = ctx.db.read()?;
            if !standard && db.knob(&name).is_none() {
                bail!("{}", unknown_custom(&name));
            }
            // A derived default can be empty (`email` with no git identity).
            match db.knob_or_default(&name).filter(|v| !v.is_empty()) {
                Some(stored) => {
                    println!("{}{}", knob_display_form(&name, &stored), override_marker(&name))
                }
                None => println!("{}", "(unset)".dimmed()),
            }
        }
        Some(value) => {
            let stored = crate::database::knob_stored_form(&name, &value)?;
            let created = ctx.db.update(|db| {
                if !standard && !custom && db.knob(&name).is_none() {
                    bail!("{}", unknown_custom(&name));
                }
                let created = !standard && db.knob(&name).is_none();
                db.set_knob(&name, stored.clone());
                Ok(created)
            })?;
            let shown = knob_display_form(&name, &stored);
            let verb = if created { "Created custom knob" } else { "Set" };
            println!("{}", format!("{verb} {} = {}.", name.bold(), shown).bright_green());
        }
    }
    Ok(())
}

/// `cactup knob` with no arguments: every standard knob, then the custom ones.
fn print_all(ctx: &Ctx) -> Res<()> {
    let db = ctx.db.read()?;
    println!("Standard knobs:");
    for spec in KNOWN_KNOBS {
        let knob = spec.name;
        let marker = override_marker(knob);
        match (db.knob(knob), db.knob_or_default(knob)) {
            (Some(stored), _) => println!("  {knob} = {}{marker}", (spec.render)(stored)),
            (None, Some(derived)) => {
                println!("  {knob} = {} {}", (spec.render)(&derived), "(derived)".dimmed())
            }
            (None, None) => println!("  {knob} {}", "(unset)".dimmed()),
        }
    }
    println!("Custom knobs:");
    let mut any = false;
    for (knob, value) in db.custom_knobs() {
        any = true;
        println!("  {knob} = {value}{}", override_marker(knob));
    }
    if !any {
        println!("  {}", "(none — create one with `cactup knob -c NAME VALUE`)".dimmed());
    }
    Ok(())
}

/// The error for a non-standard name nothing has created yet.
fn unknown_custom(name: &str) -> String {
    let known: Vec<&str> = KNOWN_KNOBS.iter().map(|s| s.name).collect();
    format!(
        "no knob named \"{name}\" (standard knobs: {}). To create a custom knob of that name, \
         pass -c/--custom: `cactup knob -c {name} VALUE`",
        known.join(", ")
    )
}

/// " (-K)" when the value shown comes from this command's overlay (§5.1).
fn override_marker(name: &str) -> String {
    if knob_overrides().contains_key(name) {
        format!(" {}", "(-K override, not stored)".dimmed())
    } else {
        String::new()
    }
}
