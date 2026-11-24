//! `peko computer`: your pekos using this computer (ADR-123).
//!
//! An owner's peko can see the screen and click and type with the
//! `Computer` tool. On macOS it needs Screen Recording and Accessibility,
//! granted to the app that started Peko (Peko Desktop, or your terminal);
//! `status` says whether they are granted and `setup` asks for them.
//! Moving the mouse or typing pauses a peko using the screen; `continue`
//! lets it go on and `stop` ends it.

use crate::commands::GlobalPaths;
use clap::Subcommand;
use peko_core::ipc::{DaemonClient, ResponsePacket};
use serde_json::Value;

/// Computer subcommands
#[derive(Subcommand)]
#[command(disable_version_flag = true)]
pub enum ComputerCommands {
    /// Whether pekos can use this computer, the permissions it needs, and
    /// which peko is using it now
    Status,

    /// Ask for the permissions pekos need to see the screen and use it
    /// (macOS: Screen Recording and Accessibility)
    Setup,

    /// Let a peko your mouse or keyboard paused go on
    Continue,

    /// Stop the peko using the screen
    Stop,
}

async fn ask(client: &DaemonClient, verb: &str) -> anyhow::Result<Value> {
    match client.computer(verb).await? {
        ResponsePacket::ComputerResult { body, .. } => Ok(body),
        ResponsePacket::Error { message, .. } => Err(anyhow::anyhow!("{message}")),
        other => Err(anyhow::anyhow!(
            "unexpected answer: {}",
            other.variant_name()
        )),
    }
}

fn granted(v: &Value) -> &'static str {
    match v.as_bool() {
        Some(true) => "granted",
        Some(false) => "not granted",
        None => "not needed",
    }
}

fn print_status(status: &Value) {
    if status["enabled"] == false {
        println!("Pekos may not use this computer: `[computer] enabled = false` in runtime.toml.");
    } else if let Some(why) = status["unavailable"].as_str() {
        println!("Pekos cannot use this computer: {why}");
    } else {
        println!(
            "Pekos can use this computer ({}).",
            status["system"].as_str().unwrap_or_default()
        );
    }
    let p = &status["permissions"];
    if p["screen_recording"].is_boolean() || p["accessibility"].is_boolean() {
        if let Some(app) = p["responsible_app"].as_str() {
            println!("Permissions go to: {app}");
        }
        println!("  Screen Recording: {}", granted(&p["screen_recording"]));
        println!("  Accessibility:    {}", granted(&p["accessibility"]));
        if p["screen_recording"] == false || p["accessibility"] == false {
            println!("Run `peko computer setup` to ask for them.");
        }
    }
    if let Some(peko) = status["paused"]["peko"].as_str() {
        println!(
            "{peko} is paused: you used the computer while it did. `peko computer continue` lets \
             it go on."
        );
    } else if let Some(peko) = status["using"]["peko"].as_str() {
        println!(
            "{peko} is using the screen: {}.",
            status["using"]["action"].as_str().unwrap_or_default()
        );
    }
}

/// Handle `peko computer`
pub async fn handle_computer(
    cmd: ComputerCommands,
    _paths: &GlobalPaths,
    json: bool,
) -> anyhow::Result<()> {
    let client = DaemonClient::connect()
        .await
        .map_err(|_| anyhow::anyhow!("Peko is not running. Start it with `peko engine start`."))?;
    let verb = match cmd {
        ComputerCommands::Status => "state",
        ComputerCommands::Setup => "setup",
        ComputerCommands::Continue => "resume",
        ComputerCommands::Stop => "stop",
    };
    let body = ask(&client, verb).await?;
    if json {
        println!("{body}");
        return Ok(());
    }
    match cmd {
        ComputerCommands::Status => print_status(&body),
        ComputerCommands::Setup => {
            let opened: Vec<&str> = body["opened"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if opened.is_empty() {
                println!("Nothing to ask for.");
            } else {
                println!(
                    "Opened System Settings for: {}. Turn Peko on there; Screen Recording takes \
                     effect after Peko restarts.",
                    opened.join(", ")
                );
            }
            print_status(&body);
        }
        ComputerCommands::Continue => {
            if body["resumed"] == true {
                println!("It goes on.");
            } else {
                println!("No peko was paused.");
            }
        }
        ComputerCommands::Stop => match body["stopped"]["peko"].as_str() {
            Some(peko) => println!("Stopped {peko}'s use of the computer."),
            None => println!("No peko was using the computer."),
        },
    }
    Ok(())
}
