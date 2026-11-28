use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TrayError {
    #[error("tray build failed: {0}")]
    BuildFailed(String),
}

pub type Result<T> = std::result::Result<T, TrayError>;

/// The favicon tile, filling the square like the icons beside it.
#[cfg(not(target_os = "macos"))]
const TRAY_ICON: &[u8] = include_bytes!("../../icons/tray.png");
/// The favicon as a menu bar template: black where the tile shows, clear
/// where the leaves are.
#[cfg(target_os = "macos")]
const TRAY_ICON: &[u8] = include_bytes!("../../icons/tray-template.png");

/// The menu's items, kept so their labels can follow the app's language.
pub struct TrayItems {
    open: MenuItem<tauri::Wry>,
    quit: MenuItem<tauri::Wry>,
}

/// The menu's labels in the language the app is shown in.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayLabels {
    open: String,
    quit: String,
}

/// Relabel the tray menu. The page owns the app's text (`src/i18n`), so it
/// sends the labels at start and whenever the language changes; until then
/// the menu shows the English it was built with.
#[tauri::command]
pub fn tray_set_labels(app: AppHandle, labels: TrayLabels) -> std::result::Result<(), String> {
    let Some(items) = app.try_state::<TrayItems>() else {
        return Ok(());
    };
    for (item, text) in [(&items.open, &labels.open), (&items.quit, &labels.quit)] {
        item.set_text(text).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Whether the tray icon exists. Without it a hidden window could not be
/// brought back, so closing the window must quit the app instead.
pub fn is_present(app: &AppHandle) -> bool {
    app.try_state::<TrayItems>().is_some()
}

/// Bring the app's window back and put it in front.
pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn build_tray(app: &AppHandle) -> Result<TrayIcon> {
    let open_i = MenuItem::with_id(app, "open", "Open", true, None::<&str>)
        .map_err(|e| TrayError::BuildFailed(e.to_string()))?;
    let separator =
        PredefinedMenuItem::separator(app).map_err(|e| TrayError::BuildFailed(e.to_string()))?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
        .map_err(|e| TrayError::BuildFailed(e.to_string()))?;

    let menu = Menu::with_items(app, &[&open_i, &separator, &quit_i])
        .map_err(|e| TrayError::BuildFailed(e.to_string()))?;

    app.manage(TrayItems {
        open: open_i.clone(),
        quit: quit_i.clone(),
    });

    // PekoHub's favicon, not the window icon: the app icon's padding
    // shrinks the sprout to a speck at tray size. macOS gets it as a
    // template image the menu bar tints (`scripts/gen-tray-icon.py`).
    let icon = tauri::image::Image::from_bytes(TRAY_ICON)
        .map_err(|e| TrayError::BuildFailed(e.to_string()))?;
    let tray = TrayIconBuilder::new()
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Peko Desktop")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray: &TrayIcon, event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)
        .map_err(|e: tauri::Error| TrayError::BuildFailed(e.to_string()))?;
    Ok(tray)
}

#[cfg(test)]
mod tests {
    use tauri::image::Image;

    fn decode(bytes: &[u8]) -> Image<'_> {
        Image::from_bytes(bytes).unwrap()
    }

    /// Windows and Linux: the favicon tile fills the 32 px square, teal
    /// to its edges, with rounded (clear) corners.
    #[test]
    fn tray_icon_is_the_favicon_tile() {
        let icon = decode(include_bytes!("../../icons/tray.png"));
        assert_eq!((icon.width(), icon.height()), (32, 32));
        let px = |x: u32, y: u32| {
            let i = ((y * icon.width() + x) * 4) as usize;
            &icon.rgba()[i..i + 4]
        };
        assert_eq!(px(0, 0)[3], 0, "rounded corner is clear");
        assert_eq!(px(16, 1), &[0, 136, 128, 255], "top edge is the teal tile");
    }

    /// macOS: a template image, so only black and clear, with clear air
    /// around the tile.
    #[test]
    fn mac_tray_icon_is_a_template() {
        let icon = decode(include_bytes!("../../icons/tray-template.png"));
        assert_eq!((icon.width(), icon.height()), (36, 36));
        for px in icon.rgba().chunks(4) {
            assert!(
                px[3] == 0 || px[..3] == [0, 0, 0],
                "template pixels are black: {px:?}"
            );
        }
        assert_eq!(icon.rgba()[(18 * 36 + 1) * 4 + 3], 0, "margin is clear");
    }
}
