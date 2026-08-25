//! `cactup releases` — list available Einstein Toolkit releases.

use super::Ctx;
use crate::manifest;
use crate::Res;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, all: bool) -> Res<()> {
    let repo = manifest::ensure_manifest_repo(&crate::CACTUP_ROOT, &ctx.globals.manifest_url)?;

    let releases = manifest::get_releases(&repo)?;

    if releases.is_empty() {
        println!("No releases found.");
        return Ok(());
    }

    let mut releases = releases.into_iter();

    if all {
        println!("{} {}", releases.next().unwrap().name.bold(), "(latest)".bold().bright_green());
        for release in releases {
            println!("{}", release.name)
        }
    } else {
        const MAX_TAGS: usize = 10;
        println!("Showing the {MAX_TAGS} most recent releases. Pass {} to see them all.", "--all".bold());

        println!("{} {}", releases.next().unwrap().name.bold(), "(latest)".bold().bright_green());

        for release in releases.take(MAX_TAGS - 1) {
            println!("{}", release.name)
        }
    }

    // Master is not a release and so is not listed above, but it is a thing
    // `install`/`refetch --release` accept — say so here, where someone
    // looking for something newer than the latest release will look.
    println!();
    println!(
        "Newer than every release above is the manifest's master branch. Pass {} to \
         {} or {} to install its tip instead of a release.",
        manifest::MASTER.bold(),
        "cactup install".bold(),
        "cactup inst refetch --release".bold()
    );

    Ok(())
}
