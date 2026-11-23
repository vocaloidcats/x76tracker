//! Optional x76 profile card. Uses x76's public profile data (no login): the user types their
//! handle once; we fetch display name, avatar, banner, badges and SteamID from
//! `https://x76.gg/api/social/users/<handle>`.

use serde_json::Value;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::time::Duration;

const SITE: &str = "https://x76.gg";

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub handle: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub banner_url: Option<String>,
    pub badges: Vec<String>,
    pub supporter: bool,
    pub verified: bool,
    pub steam_id: Option<String>,
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Accepts "name", "@name" or a pasted profile link like "https://x76.gg/u/?h=name" / "x76.gg/@name".
pub fn clean_handle(input: &str) -> String {
    let s = input.trim();
    let s = s.rsplit_once("h=").map(|(_, h)| h).unwrap_or(s);
    let s = s.rsplit_once('@').map(|(_, h)| h).unwrap_or(s);
    let s = s.split(['/', '?', '&', '#']).next().unwrap_or("");
    s.trim().to_lowercase()
}

pub fn fetch(handle: &str) -> Result<Profile, String> {
    let agent = crate::tracker::agent(Duration::from_secs(20));
    let mut resp = agent
        .get(format!("{SITE}/api/social/users/{}", handle))
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    if resp.status() == 404 {
        return Err(format!("No x76 profile called “{handle}”."));
    }
    if !resp.status().is_success() {
        return Err(format!("x76 answered {}", resp.status()));
    }
    let v: Value = resp.body_mut().read_json().map_err(|e| e.to_string())?;
    let u = v.get("user").ok_or("x76 sent an unexpected answer")?;
    let abs = |p: Option<String>| p.map(|p| if p.starts_with("http") { p } else { format!("{SITE}{p}") });
    Ok(Profile {
        handle: text(u, "handle").unwrap_or_else(|| handle.to_string()),
        display_name: text(u, "displayName").or_else(|| text(u, "handle")).unwrap_or_else(|| handle.to_string()),
        avatar_url: abs(text(u, "avatarUrl")),
        banner_url: abs(text(u, "bannerUrl")),
        badges: u
            .get("badges")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        supporter: text(u, "tier").as_deref() == Some("supporter"),
        verified: u.get("verified").and_then(Value::as_bool).unwrap_or(false),
        steam_id: text(u, "steamId64"),
    })
}

/// Downloads and decodes a picture. `circle` cuts it to a round avatar with a soft edge.
pub fn picture(url: &str, max_side: u32, circle: bool) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let agent = crate::tracker::agent(Duration::from_secs(20));
    let bytes = agent.get(url).call().ok()?.body_mut().with_config().limit(16 * 1024 * 1024).read_to_vec().ok()?;
    let mut img = image::load_from_memory(&bytes).ok()?;
    if circle {
        let side = img.width().min(img.height());
        img = img.crop_imm((img.width() - side) / 2, (img.height() - side) / 2, side, side);
    }
    if img.width() > max_side || img.height() > max_side {
        img = img.resize(max_side, max_side, image::imageops::FilterType::Triangle);
    }
    let mut rgba = img.to_rgba8();
    if circle {
        let (w, h) = rgba.dimensions();
        // a rounded square, like x76's avatars (corner radius 15% of the side), with a 1px soft edge
        let (cx, cy, half) = (w as f32 / 2.0, h as f32 / 2.0, w.min(h) as f32 / 2.0);
        let r = w.min(h) as f32 * 0.15;
        for (x, y, px) in rgba.enumerate_pixels_mut() {
            let dx = ((x as f32 + 0.5 - cx).abs() - (half - r)).max(0.0);
            let dy = ((y as f32 + 0.5 - cy).abs() - (half - r)).max(0.0);
            let d = (dx * dx + dy * dy).sqrt() - r;
            let cover = (0.5 - d).clamp(0.0, 1.0);
            px[3] = (px[3] as f32 * cover) as u8;
        }
    }
    let (w, h) = rgba.dimensions();
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), w, h))
}
