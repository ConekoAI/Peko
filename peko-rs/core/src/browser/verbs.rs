//! What a viewer may ask of a peko's browser (ADR-119 D4–D6).
//!
//! One table for every way a viewer reaches the runtime: IPC on this
//! computer, a paired phone's device verbs, and the owner's other
//! computers through PekoHub. Whoever calls has already established
//! that the viewer is the peko's owner.

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::{BrowserDesk, ControlChange};
use crate::principal::Principal;

/// The verbs, as IPC, phones and tunnels name them.
pub const VERBS: &[&str] = &["state", "frame", "control", "input", "open", "stop"];

/// The longest a `frame` call waits for a newer picture.
pub const MAX_FRAME_WAIT: Duration = Duration::from_secs(8);

/// Most input events one call applies.
const MAX_EVENTS: usize = 64;

/// Longest piece of text one input event carries.
const MAX_TEXT_CHARS: usize = 4_000;

/// Carry out `verb` for `viewer` (`local`, `device:<id>`, or
/// `computer:<runtime id>`) on `principal`'s browser.
pub async fn handle(
    desk: &BrowserDesk,
    principal: &Principal,
    viewer: &str,
    verb: &str,
    args: &Value,
) -> Result<Value> {
    let peko = desk.peko(principal);
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let number = |key: &str| args.get(key).and_then(Value::as_u64);
    match verb {
        "state" => Ok(desk.view(&peko).await),
        "frame" => {
            let tab = text("tab").ok_or_else(|| anyhow!("`frame` needs `tab`"))?;
            let size = |key: &str, default: u32| {
                number(key).map_or(default, |n| u32::try_from(n).unwrap_or(default))
            };
            let wait =
                Duration::from_millis(number("wait_ms").unwrap_or(4_000)).min(MAX_FRAME_WAIT);
            let frame = desk
                .frame(
                    &peko,
                    viewer,
                    tab,
                    (
                        size("width", super::engine::DEFAULT_WIDTH),
                        size("height", super::engine::DEFAULT_HEIGHT),
                    ),
                    number("after").unwrap_or(0),
                    wait,
                )
                .await?;
            let controller = peko.controller().map(|(viewer, _)| viewer);
            Ok(match frame {
                Some(frame) => json!({
                    "seq": frame.seq,
                    "tab": frame.tab,
                    "width": frame.width,
                    "height": frame.height,
                    "data": &*frame.data,
                    "controller": controller,
                }),
                None => json!({ "seq": Value::Null, "controller": controller }),
            })
        }
        "control" => {
            let take = args
                .get("take")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("`control` needs `take` (true or false)"))?;
            let change = desk.control(&peko, viewer, take).await?;
            if change != ControlChange::Unchanged {
                desk.audit_control(&peko, viewer, if take { "took" } else { "released" })
                    .await;
            }
            Ok(json!({ "held": take, "changed": change != ControlChange::Unchanged }))
        }
        "input" => {
            let tab = text("tab").ok_or_else(|| anyhow!("`input` needs `tab`"))?;
            let events = args
                .get("events")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("`input` needs `events`"))?;
            anyhow::ensure!(
                events.len() <= MAX_EVENTS,
                "at most {MAX_EVENTS} input events in one call"
            );
            anyhow::ensure!(
                events.iter().all(|e| e["text"]
                    .as_str()
                    .is_none_or(|t| t.chars().count() <= MAX_TEXT_CHARS)),
                "a piece of text is at most {MAX_TEXT_CHARS} characters"
            );
            desk.input(&peko, viewer, tab, events).await?;
            Ok(json!({ "ok": true }))
        }
        "open" => {
            let url = text("url").ok_or_else(|| anyhow!("`open` needs `url`"))?;
            let tab = desk.navigate(&peko, viewer, text("tab"), url).await?;
            Ok(json!({ "tab": tab }))
        }
        "stop" => Ok(json!({ "stopped": desk.stop(&peko).await })),
        other => Err(anyhow!(
            "unknown browser verb `{other}` ({})",
            VERBS.join(", ")
        )),
    }
}
