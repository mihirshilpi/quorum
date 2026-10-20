//! The menu-bar (tray) icon: a ring drawn in code with a presence dot, and a menu to set
//! presence, open the window or quit. No image files: the pixels are computed here.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Wry};

/// Icon size in pixels (menu-bar icons are 22pt; 44px covers Retina).
pub const SIZE: u32 = 44;

/// Tray menu item id prefix for presence choices.
pub const PRESENCE_PREFIX: &str = "presence:";
/// Tray menu id: show the main window.
pub const OPEN_ID: &str = "tray:open";
/// Tray menu id: quit.
pub const QUIT_ID: &str = "tray:quit";

/// Presence choices offered in the tray: (presence.set availability, label).
pub const CHOICES: &[(&str, &str)] = &[
    ("available", "Available"),
    ("busy", "Busy"),
    ("dnd", "Do not disturb"),
    ("brb", "Be right back"),
    ("away", "Appear away"),
    ("offline", "Appear offline"),
    ("reset", "Reset status"),
];

/// RGBA colour of the presence dot for a short presence id.
pub fn presence_color(presence: &str) -> [u8; 4] {
    match presence {
        "available" => [0x2e, 0xa0, 0x43, 0xff],
        "busy" | "dnd" | "do_not_disturb" => [0xd1, 0x34, 0x38, 0xff],
        "away" | "brb" | "be_right_back" => [0xe3, 0xa2, 0x1a, 0xff],
        _ => [0x8a, 0x8a, 0x8a, 0xff],
    }
}

/// Human label of a presence id.
pub fn presence_label(presence: &str) -> &'static str {
    match presence {
        "available" => "Available",
        "busy" => "Busy",
        "dnd" | "do_not_disturb" => "Do not disturb",
        "brb" | "be_right_back" => "Be right back",
        "away" => "Away",
        "offline" => "Offline",
        _ => "Status unknown",
    }
}

/// Draws the icon: a dark ring (the "Q" body) and a presence dot at the lower right with a
/// transparent gap, anti-aliased. Returns `SIZE * SIZE * 4` RGBA bytes.
pub fn draw(presence: &str) -> Vec<u8> {
    let n = SIZE as usize;
    let mut px = vec![0u8; n.saturating_mul(n).saturating_mul(4)];
    let s = SIZE as f32;
    let (cx, cy, r_out, r_in) = (s * 0.42, s * 0.42, s * 0.36, s * 0.24);
    let (dx, dy, dr, gap) = (s * 0.74, s * 0.74, s * 0.18, s * 0.06);
    let ring = [0x33, 0x33, 0x33];
    let dot = presence_color(presence);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d_ring = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            let d_dot = ((fx - dx).powi(2) + (fy - dy).powi(2)).sqrt();
            // Coverage in [0, 1] with a one-pixel soft edge.
            let ring_a = ((r_out - d_ring + 0.5).clamp(0.0, 1.0)) * ((d_ring - r_in + 0.5).clamp(0.0, 1.0));
            let cut = (d_dot - (dr + gap) + 0.5).clamp(0.0, 1.0);
            let dot_a = (dr - d_dot + 0.5).clamp(0.0, 1.0);
            let (rgb, a) = if dot_a > 0.0 { ([dot[0], dot[1], dot[2]], dot_a) } else { (ring, ring_a * cut) };
            let i = (y * n + x) * 4;
            if let Some(p) = px.get_mut(i..i + 4) {
                p.copy_from_slice(&[rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8]);
            }
        }
    }
    px
}

/// Builds the tray menu for the current presence.
///
/// # Errors
/// The platform menu could not be created.
pub fn menu(app: &AppHandle, presence: &str) -> tauri::Result<Menu<Wry>> {
    let status = MenuItem::with_id(app, "tray:status", presence_label(presence), false, None::<&str>)?;
    let choices: Vec<MenuItem<Wry>> = CHOICES
        .iter()
        .map(|(id, label)| MenuItem::with_id(app, format!("{PRESENCE_PREFIX}{id}"), *label, true, None::<&str>))
        .collect::<tauri::Result<_>>()?;
    let refs: Vec<&dyn tauri::menu::IsMenuItem<Wry>> =
        choices.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<Wry>).collect();
    let set = Submenu::with_items(app, "Set status", true, &refs)?;
    let open = MenuItem::with_id(app, OPEN_ID, "Open Quorum", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT_ID, "Quit Quorum", true, Some("CmdOrCtrl+Q"))?;
    Menu::with_items(
        app,
        &[&status, &set, &PredefinedMenuItem::separator(app)?, &open, &PredefinedMenuItem::separator(app)?, &quit],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_ring_dot_and_transparent_corners() {
        let px = draw("available");
        assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
        let at = |x: u32, y: u32| {
            let i = ((y * SIZE + x) * 4) as usize;
            px.get(i..i + 4).map(<[u8]>::to_vec).unwrap_or_default()
        };
        assert_eq!(at(0, 0).get(3), Some(&0), "corner is transparent");
        let dot = at(SIZE * 74 / 100, SIZE * 74 / 100);
        assert_eq!(dot, presence_color("available").to_vec());
        assert_ne!(draw("busy"), px);
        assert_eq!(presence_color("weird"), presence_color("offline"));
    }
}
