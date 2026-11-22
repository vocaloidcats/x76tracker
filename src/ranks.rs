//! x76's rank emblems. They're x76's artwork, so they aren't bundled with the app: they're
//! downloaded from x76.gg the first time and cached next to your history (`ranks/`).

use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::path::Path;
use std::time::Duration;

/// In tier-key order (0 nab … 7 light), matching `view::tier_key` and the UI.
pub const TIERS: [&str; 8] = ["nab", "bronze", "silver", "gold", "platinum", "diamond", "prism", "light"];
const SIZE: u32 = 128;

fn decode(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), w, h))
}

/// Cached copy if there is one, otherwise a fresh download (which is then cached).
fn one(cache: &Path, tier: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let file = cache.join(format!("{tier}-{SIZE}.webp"));
    if let Some(px) = std::fs::read(&file).ok().and_then(|b| decode(&b)) {
        return Some(px);
    }
    let agent = crate::tracker::agent(Duration::from_secs(20));
    let bytes = agent
        .get(format!("https://x76.gg/assets/ranks/{tier}-{SIZE}.webp"))
        .call()
        .ok()?
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_vec()
        .ok()?;
    let px = decode(&bytes)?;
    let _ = std::fs::create_dir_all(cache);
    let _ = std::fs::write(&file, &bytes);
    Some(px)
}

/// All eight emblems; None for any that couldn't be loaded (the UI then draws its own diamond).
pub fn load(cache: &Path) -> Vec<Option<SharedPixelBuffer<Rgba8Pixel>>> {
    TIERS.iter().map(|t| one(cache, t)).collect()
}
