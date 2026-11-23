//! `peko browser`: the browser your pekos use (ADR-119).
//!
//! Each peko drives a headless Chromium browser with a profile of its
//! own. An installed Chrome, Edge, Brave or Chromium is used; with none,
//! `install` downloads Google's Chrome for Testing into Peko's folder.
//! Stepping into a peko's browser (to sign in, or pass a check) is done
//! in the Peko app, on this computer or another of your devices.

use crate::commands::GlobalPaths;
use clap::Subcommand;
use peko_core::ipc::{DaemonClient, ResponsePacket};
use serde_json::{json, Value};

/// Browser subcommands
#[derive(Subcommand)]
#[command(disable_version_flag = true)]
pub enum BrowserCommands {
    /// Show which browser Peko uses, or a peko's open tabs
    Status {
        /// A peko, to list its tabs and who has control
        peko: Option<String>,
    },

    /// Download a browser for your pekos (about 190 MB, from Google)
    ///
    /// Only needed when this computer has no Chrome, Edge, Brave or
    /// Chromium.
    Install,

    /// Stop a peko's browser (it starts again when the peko next uses it)
    Stop {
        /// The peko
        peko: String,
    },
}

async fn ask(client: &DaemonClient, peko: &str, verb: &str) -> anyhow::Result<Value> {
    match client.browser(peko, verb, json!({})).await? {
        ResponsePacket::BrowserResult { body, .. } => Ok(body),
        ResponsePacket::Error { message, .. } => Err(anyhow::anyhow!("{message}")),
        other => Err(anyhow::anyhow!(
            "unexpected answer: {}",
            other.variant_name()
        )),
    }
}

fn megabytes(bytes: &Value) -> u64 {
    bytes.as_u64().unwrap_or(0) >> 20
}

fn print_status(status: &Value) {
    if status["available"] == true {
        println!(
            "Browser: {} ({})",
            status["path"].as_str().unwrap_or_default(),
            status["source"].as_str().unwrap_or_default()
        );
    } else if status["install"]["running"] == true {
        println!("Downloading a browser…");
    } else {
        println!(
            "No Chrome, Edge, Brave or Chromium on this computer. Install one, or run `peko \
             browser install`."
        );
    }
    if let Some(error) = status["install"]["error"].as_str() {
        println!("The last download failed: {error}");
    }
}

/// Handle `peko browser`
pub async fn handle_browser(
    cmd: BrowserCommands,
    _paths: &GlobalPaths,
    json: bool,
) -> anyhow::Result<()> {
    let client = DaemonClient::connect()
        .await
        .map_err(|_| anyhow::anyhow!("Peko is not running. Start it with `peko engine start`."))?;
    match cmd {
        BrowserCommands::Status { peko: None } => {
            let status = ask(&client, "", "status").await?;
            if json {
                println!("{status}");
            } else {
                print_status(&status);
            }
            Ok(())
        }
        BrowserCommands::Status { peko: Some(peko) } => {
            let state = ask(&client, &peko, "state").await?;
            if json {
                println!("{state}");
                return Ok(());
            }
            if state["running"] != true {
                println!("{peko}'s browser is not running.");
            }
            for tab in state["tabs"].as_array().into_iter().flatten() {
                println!(
                    "  {}  {}  {}",
                    tab["tab"].as_str().unwrap_or_default(),
                    tab["url"].as_str().unwrap_or_default(),
                    tab["title"].as_str().unwrap_or_default()
                );
            }
            if let Some(viewer) = state["controller"]["viewer"].as_str() {
                println!("You have control of it ({viewer}).");
            }
            if let Some(why) = state["handoff"]["why"].as_str() {
                println!("{peko} is waiting for you: {why}");
            }
            Ok(())
        }
        BrowserCommands::Install => {
            let mut status = ask(&client, "", "install").await?;
            let mut shown = 0;
            while status["available"] != true && status["install"]["running"] == true {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                status = ask(&client, "", "status").await?;
                let done = megabytes(&status["install"]["done_bytes"]);
                if !json && done >= shown + 20 {
                    shown = done;
                    println!(
                        "  {done} of {} MB",
                        megabytes(&status["install"]["total_bytes"])
                    );
                }
            }
            if json {
                println!("{status}");
            } else {
                print_status(&status);
            }
            anyhow::ensure!(
                status["available"] == true,
                "{}",
                status["install"]["error"]
                    .as_str()
                    .unwrap_or("the download did not finish")
            );
            Ok(())
        }
        BrowserCommands::Stop { peko } => {
            let stopped = ask(&client, &peko, "stop").await?;
            if json {
                println!("{stopped}");
            } else if stopped["stopped"] == true {
                println!("Stopped {peko}'s browser.");
            } else {
                println!("{peko}'s browser was not running.");
            }
            Ok(())
        }
    }
}
