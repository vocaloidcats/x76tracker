//! x76's game servers and their player counts, from the same public list x76.gg's server browser
//! uses. The app samples the totals every minute while it's open (servers.json, 7 days).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

pub const QUAKE: &str = "Quake Live";
pub const R5: &str = "R5Reloaded";
const KEEP_SECS: f64 = 7.0 * 86_400.0;

#[derive(Clone, Debug)]
pub struct Server {
    pub game: String,
    pub name: String,
    pub region: String,
    pub map: String,
    pub players: i64,
    pub max: i64,
    pub online: bool,
}

/// Players on x76's servers at one moment, per game.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub t: f64,
    pub ql: i64,
    pub r5: i64,
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string()
}

pub fn fetch() -> Result<Vec<Server>, String> {
    let v: Value = crate::tracker::agent(Duration::from_secs(20))
        .get("https://api.x76.gg/api/servers")
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_json()
        .map_err(|e| e.to_string())?;
    let list = v.as_array().ok_or("x76 sent an unexpected server list")?;
    Ok(list
        .iter()
        .filter_map(|s| {
            let game = text(s, "game");
            if game.is_empty() {
                return None;
            }
            let host = text(s, "hostname");
            Some(Server {
                // "x76 | Antilag LG Duel EU #1" → "Antilag LG Duel EU #1"
                name: host.strip_prefix("x76 | ").unwrap_or(&host).to_string(),
                region: text(s, "region"),
                map: text(s, "map"),
                players: s.get("players").and_then(Value::as_i64).unwrap_or(0),
                max: s.get("maxPlayers").and_then(Value::as_i64).unwrap_or(0),
                online: s.get("online").and_then(Value::as_bool).unwrap_or(false),
                game,
            })
        })
        .collect())
}

/// Total players on the online servers of one game.
pub fn total(servers: &[Server], game: &str) -> i64 {
    servers.iter().filter(|s| s.game == game && s.online).map(|s| s.players).sum()
}

pub fn load(path: &Path) -> Vec<Sample> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(path: &Path, samples: &mut Vec<Sample>) {
    let cutoff = crate::tracker::now() - KEEP_SECS;
    samples.retain(|s| s.t >= cutoff);
    if let Ok(body) = serde_json::to_vec(samples) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}
