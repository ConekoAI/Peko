//! Systems `Computer` does not support yet (ADR-123 D4: Linux is not in
//! this ADR). Every action reports that it is not available.

use std::sync::Arc;

use anyhow::{bail, Result};

use super::{Front, Image, InputEvent, Permissions, Platform, Screen, Target, WindowInfo};

const NOT_HERE: &str = "Computer is not available on this system yet: it works on macOS and \
                        Windows. Use `Bash` or an MCP server instead.";

pub struct Unsupported;

impl Platform for Unsupported {
    fn system(&self) -> &'static str {
        std::env::consts::OS
    }

    fn unavailable(&self) -> Option<String> {
        Some(NOT_HERE.into())
    }

    fn permissions(&self) -> Permissions {
        Permissions::default()
    }

    fn request_permissions(&self) -> Result<Vec<String>> {
        bail!(NOT_HERE)
    }

    fn screens(&self) -> Result<Vec<Screen>> {
        bail!(NOT_HERE)
    }

    fn windows(&self) -> Result<Vec<WindowInfo>> {
        bail!(NOT_HERE)
    }

    fn front(&self) -> Result<Front> {
        bail!(NOT_HERE)
    }

    fn raise(&self, _window: &WindowInfo) -> Result<()> {
        bail!(NOT_HERE)
    }

    fn owner_at(&self, _x: f64, _y: f64) -> Option<(u32, String)> {
        None
    }

    fn obstruction(&self) -> Option<String> {
        Some(NOT_HERE.into())
    }

    fn secure_input(&self) -> bool {
        false
    }

    fn unreachable(&self, _window: &WindowInfo) -> Option<String> {
        Some(NOT_HERE.into())
    }

    fn capture(&self, _target: Target) -> Result<Image> {
        bail!(NOT_HERE)
    }

    fn post(&self, _event: &InputEvent) -> Result<()> {
        bail!(NOT_HERE)
    }

    fn watch_input(&self, _owner_input: Arc<dyn Fn() + Send + Sync>) -> Result<()> {
        Ok(())
    }

    fn hold_display_awake(&self) -> Option<Box<dyn Send>> {
        None
    }
}
