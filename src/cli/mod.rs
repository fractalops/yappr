//! CLI dispatch and process error handling.

mod args;
mod config;
mod context;
mod devices;
mod feed;
mod hooks;
mod pairing;
mod qr;
mod questions;
mod writer;

use std::{io, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use iroh::EndpointAddr;
use yappr::{
    choices,
    client::{Denied, Unreachable},
    entry::{self, Invalid},
    ingest::Rejected,
    peers, EndpointTicket,
};

use args::{AliasCommand, Args, Commands, Format, ReadArgs};
use questions::QuestionOutcome;
use writer::Undelivered;

const ONLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// A command line that clap accepts but the command cannot use. Exits with 2,
/// like a clap usage error.
#[derive(Debug)]
pub(super) struct Usage(pub(super) String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

/// An error as text that is safe for a terminal. An error can carry text
/// from another endpoint, such as the reason why it closed a connection.
pub(super) fn shown(error: &anyhow::Error) -> String {
    yappr::proto::printable(&format!("{error:#}"))
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    })
}

pub(super) async fn run() -> Result<()> {
    let args = Args::parse();
    let common = &args.common;
    match args.command {
        Commands::Post(args) => writer::post(common, args).await,
        Commands::Start(args) => writer::start(common, args, writer::Pairing::First).await,
        Commands::Init(args) => writer::start(common, args, writer::Pairing::Always).await,
        Commands::Serve(args) => writer::start(common, args, writer::Pairing::Never).await,
        Commands::On(args) => {
            tokio::select! {
                res = feed::on(common, args) => res,
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
        Commands::Read(args) => {
            tokio::select! {
                res = feed::read(common, args) => res,
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
        Commands::Follow(args) => {
            let args = ReadArgs {
                follow: true,
                ..args
            };
            tokio::select! {
                res = feed::read(common, args) => res,
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
        Commands::Allow {
            id,
            alias,
            receives,
        } => devices::allow(common, id, alias, receives),
        Commands::Key(args) => devices::key(common, args),
        Commands::Hook(args) => {
            hooks::hook(common, args).await;
            Ok(())
        }
        Commands::Deny { id } => devices::deny(common, id),
        Commands::Allowed => {
            for id in common.store()?.allow_list().load()? {
                println!("{id}");
            }
            Ok(())
        }
        Commands::Id => {
            println!("{}", common.secret()?.public());
            Ok(())
        }
        Commands::Ask(args) => {
            if args.choices.len() < 2 {
                return Err(Usage(
                    "give two or more choices, such as --choice retry --choice skip. \
                     To ask for approval, use: yappr approve"
                        .into(),
                )
                .into());
            }
            questions::ask(common, args.question, args.choices, None).await
        }
        Commands::Approve(args) => {
            questions::ask(
                common,
                args.question,
                choices::approval_options(),
                Some(args.unlock),
            )
            .await
        }
        Commands::Questions { output } => {
            let text = Format::is_text(output);
            for state in choices::list(&common.store()?.entries()?) {
                if text {
                    println!("{}", questions::question_line(&state));
                } else {
                    println!("{}", serde_json::to_string(&state)?);
                }
            }
            Ok(())
        }
        Commands::Wait { id } => {
            questions::wait(common, &questions::local_question(common, id)?).await
        }
        Commands::Cancel { id } => {
            choices::cancel(&common.store()?, &questions::local_question(common, id)?)
        }
        Commands::Devices => devices::devices(common),
        Commands::Status { output } => devices::status(common, Format::is_text(output)),
        Commands::Answer {
            ticket,
            id,
            value,
            choice,
        } => {
            let ticket = EndpointTicket::new(peers::resolve(&common.dir()?, &ticket)?);
            let id = questions::remote_question(common, &ticket, id).await?;
            let value = value.or(choice).context("give the value of a choice")?;
            questions::answer(common, ticket, id, value).await
        }
        Commands::Retry => writer::retry(common).await,
        Commands::Config {
            name,
            default_relays,
            mdns,
            hooks,
            output,
        } => config::run(common, name, default_relays, mdns, hooks, output),
        Commands::Pair(args) => pairing::pair(common, args).await,
        Commands::Join {
            invitation,
            name,
            alias,
        } => pairing::join(common, &invitation, name, alias).await,
        Commands::Alias { command } => match command {
            AliasCommand::Set { name, target } => peers::set(&common.dir()?, &name, &target),
            AliasCommand::Remove { name } => peers::remove(&common.dir()?, &name),
        },
        Commands::Health { target, output } => {
            devices::probe(common, &target, Format::is_text(output)).await
        }
        Commands::Ticket => {
            let addr = EndpointAddr::new(common.secret()?.public());
            println!("{}", EndpointTicket::new(addr));
            Ok(())
        }
    }
}

/// The exit code for an error. docs/reference.md documents these codes.
pub(super) fn exit_code(err: &anyhow::Error) -> i32 {
    if is_broken_pipe(err) {
        // the reader of stdout left, for example "yappr read ... | head"
        0
    } else if let Some(outcome) = err.downcast_ref::<QuestionOutcome>() {
        outcome.code
    } else if err.downcast_ref::<Usage>().is_some() {
        2
    } else if let Some(error) = err.downcast_ref::<choices::ChoiceError>() {
        match error.code.as_str() {
            "denied" => 3,
            "expired" => 6,
            "cancelled" => 8,
            _ => 4,
        }
    } else if err.downcast_ref::<Denied>().is_some() {
        3
    } else if err.downcast_ref::<Invalid>().is_some()
        || err.downcast_ref::<entry::BadSignature>().is_some()
        || err.downcast_ref::<Rejected>().is_some()
        || err.downcast_ref::<yappr::client::OutOfOrder>().is_some()
    {
        4
    } else if err.downcast_ref::<Unreachable>().is_some()
        || err.downcast_ref::<Undelivered>().is_some()
    {
        5
    } else {
        1
    }
}
