//! `cactup test` — dispatch into the test-suite subsystem (spec §11).

use super::Ctx;
use crate::args::{TestCommand, TestSimCommand};
use crate::testsuite;
use crate::Res;

pub fn dispatch(ctx: &Ctx, cmd: TestCommand) -> Res<()> {
    match cmd {
        TestCommand::Run(args) => testsuite::run::start(ctx, args, false),
        TestCommand::Submit(args) => testsuite::run::start(ctx, args, true),
        TestCommand::Clean => testsuite::manage::clean(ctx),
        TestCommand::Sim(sub) => match sub {
            TestSimCommand::Show { name, long, all } => {
                testsuite::manage::show(ctx, name.as_deref(), long, all)
            }
            TestSimCommand::Stop { name, force } => testsuite::manage::stop(ctx, &name, force),
            TestSimCommand::Delete { name, force, purge } => {
                testsuite::manage::delete(ctx, &name, force, purge)
            }
        },
    }
}
