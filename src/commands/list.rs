//! `cactup list` — list available Einstein Toolkit releases.

use super::Ctx;
use crate::manifest;
use crate::Res;
use colored::Colorize;

pub fn dispatch(ctx: &Ctx, all: bool) -> Res<()> {
    let repo = manifest::ensure_manifest_repo(&crate::CACTUP_ROOT, &ctx.globals.manifest_url)?;

    let tags = manifest::get_tags(&repo)?;

    if tags.is_empty() {
        println!("No releases found.");
        return Ok(());
    }

    let mut tags = tags.into_iter();

    if all {
        println!("{} {}", tags.next().unwrap().short_name.bold(), "(latest)".bold().bright_green());
        for tag in tags {
            println!("{}", tag.short_name)
        }
    } else {
        const MAX_TAGS: usize = 10;
        println!("Showing the {MAX_TAGS} most recent releases. Pass {} to see them all.", "--all".bold());

        println!("{} {}", tags.next().unwrap().short_name.bold(), "(latest)".bold().bright_green());

        for tag in tags.take(MAX_TAGS - 1) {
            println!("{}", tag.short_name)
        }
    }

    Ok(())
}
