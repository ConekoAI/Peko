//! A computer that exists only in memory, for the desk's tests: windows
//! that can be moved, resized and put in front, a log of what was posted,
//! and a way to play the owner touching the mouse.

use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::{Front, Image, InputEvent, Permissions, Platform, Rect, Screen, Target, WindowInfo};

#[derive(Default)]
pub struct FakeState {
    pub screens: Vec<Screen>,
    /// Front to back.
    pub windows: Vec<WindowInfo>,
    pub posted: Vec<InputEvent>,
    /// Whether `raise` brings a window forward.
    pub raise_works: bool,
    pub raised: Vec<u64>,
    pub obstruction: Option<String>,
    pub secure_input: bool,
    pub awake_holds: usize,
    owner_input: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Called after each post (to play the owner mid-action).
    pub after_post: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}

#[derive(Default)]
pub struct FakePlatform {
    pub state: Mutex<FakeState>,
}

/// Dropped when the fake's display hold ends.
struct AwakeHold(Arc<FakePlatform>);

impl Drop for AwakeHold {
    fn drop(&mut self) {
        self.0.state().awake_holds -= 1;
    }
}

impl FakePlatform {
    /// One 1440×900 screen at scale 2, with two windows: `Peko` (id 1,
    /// pid 100) in front of `Finder` (id 2, pid 200).
    #[must_use]
    pub fn new() -> Arc<Self> {
        let fake = Arc::new(Self::default());
        {
            let mut st = fake.state();
            st.raise_works = true;
            st.screens = vec![Screen {
                id: 1,
                bounds: Rect::new(0.0, 0.0, 1440.0, 900.0),
                scale: 2.0,
                main: true,
            }];
            st.windows = vec![
                WindowInfo {
                    id: 1,
                    pid: 100,
                    app: "Peko".into(),
                    title: "Peko".into(),
                    bounds: Rect::new(100.0, 50.0, 800.0, 600.0),
                    minimized: false,
                },
                WindowInfo {
                    id: 2,
                    pid: 200,
                    app: "Finder".into(),
                    title: "Downloads".into(),
                    bounds: Rect::new(300.0, 300.0, 600.0, 400.0),
                    minimized: false,
                },
            ];
        }
        fake
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Put window `id` in front.
    pub fn bring_front(&self, id: u64) {
        let mut st = self.state();
        if let Some(i) = st.windows.iter().position(|w| w.id == id) {
            let w = st.windows.remove(i);
            st.windows.insert(0, w);
        }
    }

    /// The owner touches the mouse.
    pub fn owner_touches(&self) {
        let cb = self.state().owner_input.clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    pub fn posted(&self) -> Vec<InputEvent> {
        self.state().posted.clone()
    }

    pub fn window_mut(&self, id: u64, f: impl FnOnce(&mut WindowInfo)) {
        let mut st = self.state();
        if let Some(w) = st.windows.iter_mut().find(|w| w.id == id) {
            f(w);
        }
    }
}

impl Platform for Arc<FakePlatform> {
    fn system(&self) -> &'static str {
        "fake"
    }

    fn unavailable(&self) -> Option<String> {
        None
    }

    fn permissions(&self) -> Permissions {
        Permissions::default()
    }

    fn request_permissions(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn screens(&self) -> Result<Vec<Screen>> {
        Ok(self.state().screens.clone())
    }

    fn windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(self.state().windows.clone())
    }

    fn front(&self) -> Result<Front> {
        let st = self.state();
        let w = st.windows.iter().find(|w| !w.minimized);
        Ok(Front {
            pid: w.map_or(0, |w| w.pid),
            app: w.map_or_else(String::new, |w| w.app.clone()),
            window: w.map(|w| w.id),
        })
    }

    fn raise(&self, window: &WindowInfo) -> Result<()> {
        let works = {
            let mut st = self.state();
            st.raised.push(window.id);
            st.raise_works
        };
        if works {
            self.window_mut(window.id, |w| w.minimized = false);
            self.bring_front(window.id);
        }
        Ok(())
    }

    fn owner_at(&self, x: f64, y: f64) -> Option<(u32, String)> {
        self.state()
            .windows
            .iter()
            .filter(|w| !w.minimized)
            .find(|w| w.bounds.contains(x, y))
            .map(|w| (w.pid, w.app.clone()))
    }

    fn obstruction(&self) -> Option<String> {
        self.state().obstruction.clone()
    }

    fn secure_input(&self) -> bool {
        self.state().secure_input
    }

    fn unreachable(&self, _window: &WindowInfo) -> Option<String> {
        None
    }

    fn capture(&self, target: Target) -> Result<Image> {
        let st = self.state();
        let (w, h) = match target {
            Target::Screen(id) => {
                let s = st
                    .screens
                    .iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| anyhow::anyhow!("no screen"))?;
                (s.bounds.width * s.scale, s.bounds.height * s.scale)
            }
            Target::Window(id) => {
                let win = st
                    .windows
                    .iter()
                    .find(|w| w.id == id)
                    .ok_or_else(|| anyhow::anyhow!("no window"))?;
                (win.bounds.width * 2.0, win.bounds.height * 2.0)
            }
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(Image::filled(w as u32, h as u32, [200, 200, 200, 255]))
    }

    fn post(&self, event: &InputEvent) -> Result<()> {
        let after = {
            let mut st = self.state();
            st.posted.push(event.clone());
            st.after_post.clone().map(|f| (f, st.posted.len()))
        };
        if let Some((f, n)) = after {
            f(n);
        }
        Ok(())
    }

    fn watch_input(&self, owner_input: Arc<dyn Fn() + Send + Sync>) -> Result<()> {
        self.state().owner_input = Some(owner_input);
        Ok(())
    }

    fn hold_display_awake(&self) -> Option<Box<dyn Send>> {
        self.state().awake_holds += 1;
        Some(Box::new(AwakeHold(Arc::clone(self))))
    }
}
