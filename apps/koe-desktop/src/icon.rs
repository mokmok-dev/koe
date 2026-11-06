//! The menu bar icon drawn from code, so the app ships no image assets.

use tray_icon::Icon;

use crate::tray::TrayStatus;

const SIZE: u32 = 32;
const BYTES: usize = 32 * 32 * 4;

/// Draws a microphone in the status colour.
///
/// # Errors
///
/// Returns the icon error when the RGBA buffer does not match the size.
pub fn status_icon(status: TrayStatus) -> Result<Icon, String> {
    let [red, green, blue] = colour(status);
    let mut rgba = vec![0_u8; BYTES];
    for y in 0..SIZE {
        for x in 0..SIZE {
            if !inside_microphone(x, y) {
                continue;
            }
            let offset =
                (usize::try_from(y).unwrap_or(0) * 32 + usize::try_from(x).unwrap_or(0)) * 4;
            rgba[offset] = red;
            rgba[offset + 1] = green;
            rgba[offset + 2] = blue;
            rgba[offset + 3] = u8::MAX;
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).map_err(|error| error.to_string())
}

const fn colour(status: TrayStatus) -> [u8; 3] {
    match status {
        TrayStatus::Ready => [138, 138, 147],
        TrayStatus::Starting => [240, 180, 41],
        TrayStatus::Recording => [255, 90, 95],
        TrayStatus::Saving => [88, 166, 255],
        TrayStatus::Saved => [126, 231, 135],
        TrayStatus::Failed => [255, 123, 114],
    }
}

fn inside_microphone(x: u32, y: u32) -> bool {
    let x = i32::try_from(x).unwrap_or(0);
    let y = i32::try_from(y).unwrap_or(0);
    let xf = f32::from(i16::try_from(x).unwrap_or(0));
    let yf = f32::from(i16::try_from(y).unwrap_or(0));
    let capsule = {
        let dx = (xf - 16.0) / 5.5;
        let dy = (yf - 13.0) / 8.0;
        dy.mul_add(dy, dx * dx) <= 1.0
    };
    let arc = {
        let dx = xf - 16.0;
        let dy = yf - 15.0;
        let radius = dy.mul_add(dy, dx * dx);
        y >= 15 && (49.0..=81.0).contains(&radius)
    };
    let stem = (15..17).contains(&x) && (23..28).contains(&y);
    let base = (11..21).contains(&x) && (28..30).contains(&y);
    capsule || arc || stem || base
}
