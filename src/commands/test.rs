//! `cactup test` — dispatch into the test-suite subsystem (spec §11).

use super::Ctx;
use crate::args::{TestCommand, TestSimCommand};
use crate::testsuite;
use crate::Res;

pub fn dispatch(ctx: &Ctx, cmd: TestCommand) -> Res<()> {
    match cmd {
        TestCommand::Build { name, opts } => testsuite::config::build(ctx, name, opts),
        TestCommand::Show { name } => testsuite::config::show(ctx, name.as_deref()),
        TestCommand::Use { name } => testsuite::config::use_cmd(ctx, &name),
        TestCommand::Delete { name, force } => testsuite::config::delete(ctx, &name, force),
        TestCommand::Run(args) => testsuite::run::start(ctx, args, false),
        TestCommand::Submit(args) => testsuite::run::start(ctx, args, true),
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
