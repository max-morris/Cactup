//! `cactup test` — dispatch into the test-suite subsystem (spec §11).

use super::Ctx;
use crate::args::TestCommand;
use crate::testsuite;
use crate::Res;

pub fn dispatch(ctx: &Ctx, cmd: TestCommand) -> Res<()> {
    match cmd {
        TestCommand::Run(args) => testsuite::run::start(ctx, args, false),
        TestCommand::Submit(args) => testsuite::run::start(ctx, args, true),
        TestCommand::Clean => testsuite::manage::clean(ctx),
        TestCommand::List { long, all } => testsuite::manage::list(ctx, long, all),
        TestCommand::Show { name } => testsuite::manage::show(ctx, &name),
        TestCommand::Log { name, follow, follow_out, follow_err } => {
            let mode = crate::tail::FollowMode::from_flags(follow, follow_out, follow_err);
            testsuite::manage::log_cmd(ctx, &name, mode)
        }
        TestCommand::Stop { name, force } => testsuite::manage::stop(ctx, &name, force),
        TestCommand::Delete { name, force, purge } => {
            testsuite::manage::delete(ctx, &name, force, purge)
        }
    }
}
