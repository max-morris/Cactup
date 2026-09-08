//! `cactup show` — the aggregate state view (spec §3): active installation,
//! active config, and machine, each best-effort so one missing piece doesn't
//! hide the rest. Per-subsystem detail lives in `installation show` /
//! `config show` / `machine show`.

use super::{installation, Ctx};
use crate::installation::Installation;
use crate::Res;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx) -> Res<()> {
    println!("{}", "Installation".bold());
    match installation::show_installation(ctx, None) {
        Ok(()) => {}
        Err(e) => println!("{}", format!("{e}").yellow()),
    }

    println!();
    println!("{}", "Active config".bold());
    match Installation::resolve(ctx).and_then(|inst| super::config::show(ctx, &inst, None)) {
        Ok(()) => {}
        Err(e) => println!("{}", format!("{e}").yellow()),
    }

    println!();
    println!("{}", "Machine".bold());
    if let Err(e) = super::machine::show_cached_summary(ctx) {
        println!("{}", format!("{e}").yellow());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Db;

    #[test]
    fn dispatch_never_fails_with_nothing_configured() {
        let dbdir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            globals: crate::args::GlobalOpts {
                verbose: false,
                trace: false,
                manifest_url: String::new(),
                mdb_path: None,
                machine: None,
                installation: None,
                hostname: None,
                knob: Vec::new(),
            },
            db: Db::in_dir(dbdir.path()),
        };
        // Every section degrades to a yellow note instead of erroring out.
        dispatch(&ctx).unwrap();
    }
}
