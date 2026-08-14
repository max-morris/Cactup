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
        // A refetch with an explicit source moved the tree off its
        // install-time provenance (§2.1). Shown only when it actually
        // differs — refetching the same release back is not news.
        match (&installation.current_release, &installation.current_thornlist) {
            (Some(current), _) if Some(current) != installation.release.as_ref() => {
                print!(", now on {}", current.bold())
            }
            (None, Some(current)) if Some(current) != installation.thornlist.as_ref() => {
                print!(", now on thornlist {}", current.bold())
            }
            _ => {}
        }
        // §2.1's `unfetched_repos`: non-empty means the on-disk tree only
        // partially conforms to the thornlist named above — some repos were
        // skipped (dirty) or failed the last refetch. A failure is an error
        // the user did not ask for, so it reads angrier than a mere skip.
        if !installation.unfetched_repos.is_empty() {
            let failed = installation.failed_repo_count();
            let skipped = installation.skipped_repo_count();
            if failed > 0 {
                print!(
                    "{}",
                    format!(", partial ({failed} repo(s) FAILED, {skipped} skipped)").bright_red()
                );
            } else {
                print!("{}", format!(", partial ({skipped} repo(s) not fetched)").yellow());
            }
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
