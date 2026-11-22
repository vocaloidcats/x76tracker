//! Opponents from R5Reloaded's own log. x76tracker's game-script patch writes one line to
//! `platform/logs/<session>/script.log` for each of your kills and deaths:
//!
//!   [2026-11-21 12:10:57.444] [130.738] Script(C):[x76tracker]\tkill\t<attacker>\t<victim>\t<damageSourceId>\tdead
//!
//! This module finds the game, follows the newest log, and keeps the fights in `fights.json`.

use chrono::{Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Current marker, plus the one older patches wrote (still read, so earlier fights count).
const MARKERS: [&str; 2] = ["[x76tracker]\t", "[x76EloTracker]\t"];
const MAX_FIGHTS: usize = 50_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Fight {
    /// When it happened (seconds since 1970, from the game's log clock).
    pub t: f64,
    /// "kill" (you won the fight) or "death" (you lost it).
    pub kind: String,
    /// Your in-game name at the time (Quake Live: your SteamID, from the account's folder).
    pub me: String,
    /// The other player; empty if the game couldn't tell who killed you.
    pub opponent: String,
    #[serde(default)]
    pub weapon: i64,
    /// The opponent's and your numbers from the last ladder check before the fight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opp_elo: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opp_points: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opp_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opp_rank: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub my_elo: Option<f64>,
    /// Quake Live: the opponent's SteamID (their ladder id), found by name at that ladder check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opp_id: Option<String>,
    /// Quake Live: your and the opponent's accuracy in the fight, as the x76 server prints it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub my_acc: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opp_acc: Option<f64>,
    /// Quake Live: where in the console log it was ("<session line>@<byte>"); its log has no times,
    /// so this is what tells fights apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
}

impl Fight {
    fn key(&self) -> (i64, String, String) {
        ((self.t * 1000.0).round() as i64, self.kind.clone(), self.opponent.clone())
    }
}

/// Where we are in the game's log.
#[derive(Default)]
pub struct Tail {
    pub game_dir: Option<PathBuf>,
    file: Option<PathBuf>,
    pos: u64,
    last_find: f64,
}

/// R5Reloaded installs to "<drive>:\R5R Library\<channel>" by default; check the usual spots.
fn find_game_dir(hint: Option<&str>) -> Option<PathBuf> {
    let is_game = |p: &Path| p.join("r5apex.exe").is_file() && p.join("platform").is_dir();
    if let Some(h) = hint.map(PathBuf::from).filter(|p| is_game(p)) {
        return Some(h);
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for letter in 'C'..='Z' {
        for base in ["R5R Library", "Program Files\\R5R Library", "Games\\R5R Library", "Program Files (x86)\\R5R Library"] {
            let lib = PathBuf::from(format!("{letter}:\\{base}"));
            if let Ok(entries) = std::fs::read_dir(&lib) {
                found.extend(entries.flatten().map(|e| e.path()).filter(|p| is_game(p)));
            }
        }
        for plain in ["R5Reloaded", "Games\\R5Reloaded"] {
            let p = PathBuf::from(format!("{letter}:\\{plain}"));
            if is_game(&p) {
                found.push(p);
            }
        }
    }
    // several installs: the one played most recently
    found.into_iter().max_by_key(|p| newest_log(p).and_then(|l| l.metadata().ok()?.modified().ok()))
}

fn newest_log(game_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(game_dir.join("platform").join("logs"))
        .ok()?
        .flatten()
        .map(|e| e.path().join("script.log"))
        .filter(|p| p.is_file())
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}

fn parse_line(line: &str) -> Option<Fight> {
    let rest = MARKERS.iter().find_map(|m| line.find(m).map(|i| &line[i + m.len()..]))?;
    let mut f = rest.trim_end().split('\t');
    let (kind, attacker, victim) = (f.next()?, f.next()?, f.next()?);
    let weapon = f.next().and_then(|w| w.parse().ok()).unwrap_or(0);
    let stamp = line.get(1..24)?; // "2026-11-21 12:10:57.444"
    let when = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S%.3f").ok()?;
    let t = Local.from_local_datetime(&when).earliest()?.timestamp_millis() as f64 / 1000.0;
    let (me, opponent) = match kind {
        "kill" => (attacker, victim),
        "death" => (victim, attacker),
        _ => return None,
    };
    Some(Fight { t, kind: kind.into(), me: me.into(), opponent: opponent.into(), weapon, ..Default::default() })
}

/// Reads anything new in the game's log. Returns the fights not seen before.
pub fn scan(tail: &mut Tail, hint: Option<&str>, known: &[Fight]) -> Vec<Fight> {
    let now = crate::tracker::now();
    if tail.game_dir.is_none() && now - tail.last_find > 60.0 {
        tail.last_find = now;
        tail.game_dir = find_game_dir(hint);
    }
    let Some(dir) = tail.game_dir.clone() else { return Vec::new() };
    let Some(log) = newest_log(&dir) else { return Vec::new() };
    if tail.file.as_ref() != Some(&log) {
        tail.file = Some(log.clone()); // new game session: read it from the start
        tail.pos = 0;
    }
    let Ok(mut file) = std::fs::File::open(&log) else { return Vec::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < tail.pos {
        tail.pos = 0; // the log was restarted
    }
    if len == tail.pos || file.seek(SeekFrom::Start(tail.pos)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    // only consume whole lines; a half-written last line is read next time
    let complete = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    tail.pos += complete as u64;
    let seen: HashSet<_> = known.iter().map(Fight::key).collect();
    String::from_utf8_lossy(&buf[..complete])
        .lines()
        .filter_map(parse_line)
        .filter(|f| !seen.contains(&f.key()))
        .collect()
}

pub fn load(path: &Path) -> Vec<Fight> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(path: &Path, fights: &mut Vec<Fight>) {
    if fights.len() > MAX_FIGHTS {
        let extra = fights.len() - MAX_FIGHTS;
        fights.drain(..extra);
    }
    if let Ok(body) = serde_json::to_vec(fights) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}
