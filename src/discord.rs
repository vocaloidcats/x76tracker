//! Discord: settings, the profile widget and the bot token.
//!
//! The widget part is for people who already have a Discord app with a profile widget (Discord
//! stopped giving widgets to new apps). The app's widget editor decides the layout; any field set
//! to "User Data" reads a key, and x76tracker fills those keys with x76 stats through
//! `PATCH /applications/{app}/users/{user}/identities/0/profile`, authorised by the app's bot token.

use crate::tracker::{now, Core, Game};
use crate::view::{player_stats, thousands, tier_key, PlayerStats};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// x76tracker's own Discord application, for Rich Presence when no widget app is set up.
pub const APP_ID: &str = "1573939617055637524";
const API: &str = "https://discord.com/api/v9";
/// Discord wants this form of User-Agent on bot-token requests (and blocks browser ones).
const BOT_AGENT: &str = "DiscordBot (https://github.com/discord/discord-api-docs, 1.0.0)";
/// Discord says widgets aren't meant for real-time data: automatic updates at most this often.
pub const MIN_GAP: f64 = 300.0;
/// "Update now" and settings changes: at most this often.
const MANUAL_GAP: f64 = 10.0;
const TOKEN_FILE: &str = "discord-token.bin";

// ------------------------------------------------------------------ settings
/// One User Data field (or a value/label pair, for stat tiles) in the widget's layout.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Slot {
    /// surface/component(/field): where it sits, stable between reads
    pub path: String,
    /// "tile" (value + label), "text" or "image"
    pub kind: String,
    pub surface: String,
    pub name: String,
    /// The User Data key (the value's key, for tiles; the first User Data field, for cards).
    pub key: String,
    pub label_key: String,
    /// Cards (collection items): the keys of their name, description and image, where those
    /// are User Data ("" where the editor has fixed content).
    pub name_key: String,
    pub desc_key: String,
    pub image_key: String,
    /// Progress bars: the max value's key (the current value's key is `key`).
    pub max_key: String,
    pub fallback: String,
}

/// A text field that's fixed in Discord's widget editor (not User Data); shown for reference.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct StaticField {
    pub path: String,
    pub surface: String,
    pub name: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Rich Presence on/off.
    pub rpc: bool,
    /// Whose numbers Discord shows (favourite ids; empty = the first tracked player of that game).
    pub r5_player: String,
    pub ql_player: String,
    /// Widget updates on/off.
    pub widget: bool,
    pub app_id: String,
    pub user_id: String,
    /// Run Rich Presence under the widget's app, so its "while playing" panel shows.
    pub widget_rpc: bool,
    pub widget_name: String,
    pub layout: Vec<Slot>,
    pub statics: Vec<StaticField>,
    /// Chosen source per slot key, custom texts per slot key, and static texts seen in earlier
    /// reads (by path), used as the starting custom text once a field is switched to User Data.
    pub sources: BTreeMap<String, String>,
    pub custom: BTreeMap<String, String>,
    pub remembered: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rpc: false,
            r5_player: String::new(),
            ql_player: String::new(),
            widget: false,
            app_id: String::new(),
            user_id: String::new(),
            widget_rpc: true,
            widget_name: String::new(),
            layout: Vec::new(),
            statics: Vec::new(),
            sources: BTreeMap::new(),
            custom: BTreeMap::new(),
            remembered: BTreeMap::new(),
        }
    }
}

pub fn valid_id(s: &str) -> bool {
    (15..=21).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
}

impl Config {
    pub fn load(core: &Core) -> Config {
        core.state.settings.get("discord").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    pub fn save(&self, core: &mut Core) {
        if let Ok(v) = serde_json::to_value(self) {
            core.set_setting("discord", v);
        }
    }

    pub fn widget_ready(&self) -> bool {
        valid_id(&self.app_id) && valid_id(&self.user_id) && !self.layout.is_empty()
    }

    /// The app Rich Presence runs under.
    pub fn rpc_app(&self) -> String {
        if self.widget_rpc && valid_id(&self.app_id) { self.app_id.clone() } else { APP_ID.into() }
    }

    /// The tracked player whose numbers Discord shows for a game.
    pub fn player(&self, core: &Core, game: Game) -> Option<String> {
        let chosen = match game {
            Game::R5 => &self.r5_player,
            Game::Ql => &self.ql_player,
        };
        let mut favs = core.state.favorites.iter().filter(|f| f.game == game.id());
        let first = favs.clone().next();
        favs.find(|f| &f.id == chosen).or(first).map(|f| f.id.clone())
    }

    pub fn source_of(&self, slot: &Slot) -> String {
        // a choice saved for another kind of field (the layout changed under the same key) doesn't count
        if let Some(s) = self.sources.get(&slot.key).filter(|s| options(&slot.kind).iter().any(|(id, _)| id == *s)) {
            return s.clone();
        }
        match slot.kind.as_str() {
            "progress" => "r5.progress_tier".into(),
            // the progress layout's objective: rank, distance to the next tier, the last 48 hours
            "card" if self.layout.iter().any(|s| s.kind == "progress") => "r5.card_summary".into(),
            "tile" | "card" => {
                let n = self.layout.iter().take_while(|s| s.key != slot.key).filter(|s| s.kind == slot.kind).count();
                let defaults: &[&str] = if slot.kind == "tile" { &DEFAULT_TILES } else { &DEFAULT_CARDS };
                defaults.get(n).copied().unwrap_or("none").into()
            }
            "image" => "none".into(),
            _ if slot.path.starts_with("activity_accessory") => "playing.summary".into(),
            _ if slot.path.starts_with("mini_profile") => "r5.summary".into(),
            _ => "custom".into(),
        }
    }

    pub fn custom_of(&self, slot: &Slot) -> String {
        self.custom
            .get(&slot.key)
            .or_else(|| self.remembered.get(&slot.path))
            .cloned()
            .unwrap_or_else(|| slot.fallback.clone())
    }
}

// ------------------------------------------------------------------ live state (not saved)
#[derive(Default)]
pub struct Live {
    /// The bot token, decrypted (it's stored encrypted in discord-token.bin).
    pub token: Option<String>,
    /// The game running on this PC right now, and since when.
    pub playing: Option<Game>,
    pub playing_since: f64,
    /// Rich Presence: shown right now, or why not.
    pub rpc_showing: bool,
    pub rpc_error: Option<String>,
    pub rpc_user: Option<String>,
    /// Widget: last attempt, last success, what was sent, the last error.
    pub sent_at: Option<f64>,
    pub ok_at: Option<f64>,
    pub sent: Vec<Value>,
    pub error: Option<String>,
    /// A settings change or "Update now": push once this time has passed.
    pub soon: Option<f64>,
    pub layout_busy: bool,
    pub layout_note: Option<(bool, String)>,
}

// ------------------------------------------------------------------ what can go in a field
const DEFAULT_TILES: [&str; 6] = ["r5.elo", "r5.points", "r5.rank", "r5.elo_48h", "r5.record_48h", "r5.best_win"];
const DEFAULT_CARDS: [&str; 4] = ["r5.card_rank", "r5.card_change", "r5.card_record", "r5.card_best"];

/// (id, menu text). Per-game stats are listed once and expanded for R5 and QL.
const STATS: [(&str, &str); 10] = [
    ("elo", "Elo"),
    ("points", "points"),
    ("rank", "rank"),
    ("tier", "tier"),
    ("elo_48h", "Elo change, 48h"),
    ("points_48h", "points change, 48h"),
    ("record_48h", "wins and losses, 48h"),
    ("kd", "K/D"),
    ("peak", "peak Elo (tracked)"),
    ("best_win", "best win, 48h"),
];
const LINES: [(&str, &str); 3] = [("summary", "summary line"), ("recent_line", "48h line"), ("summary_48h", "48h summary")];
/// Progress bars: (current, max).
const PROGRESS: [(&str, &str); 2] = [("progress_tier", "progress to next tier"), ("progress_winrate", "win rate, 48h")];
/// Collection cards: a picture, a name and a description each.
const CARDS: [(&str, &str); 6] = [
    ("card_summary", "rank and 48h summary"),
    ("card_rank", "rank (x76 emblem)"),
    ("card_change", "Elo change, 48h"),
    ("card_record", "wins and losses, 48h"),
    ("card_best", "best win, 48h"),
    ("card_peak", "peak Elo"),
];

/// Pictures for the cards: Microsoft's Fluent Emoji (MIT licence), 256×256 PNGs.
const FLUENT: &str = "https://raw.githubusercontent.com/microsoft/fluentui-emoji/main/assets";
fn emoji(name: &str) -> String {
    let file = name.to_lowercase().replace(' ', "_");
    format!("{FLUENT}/{}/3D/{file}_3d.png", name.replace(' ', "%20"))
}

fn short(g: Game) -> &'static str {
    match g {
        Game::R5 => "R5",
        Game::Ql => "QL",
    }
}

fn per_game(list: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for g in [Game::R5, Game::Ql] {
        for (id, menu) in list {
            if (*id == "best_win" || *id == "card_best") && g != Game::R5 {
                continue; // the widget's best win stays R5Reloaded's
            }
            out.push((format!("{}.{id}", g.id()), format!("{} {menu}", short(g))));
        }
    }
    out
}

/// Choices for a slot kind: (source id, menu text).
pub fn options(kind: &str) -> Vec<(String, String)> {
    let none = ("none".to_string(), "Nothing (Discord's fallback)".to_string());
    match kind {
        "tile" => std::iter::once(none).chain(per_game(&STATS)).collect(),
        "card" => std::iter::once(none).chain(per_game(&CARDS)).collect(),
        "progress" => std::iter::once(none).chain(per_game(&PROGRESS)).collect(),
        "image" => vec![
            none,
            ("playing.emblem".into(), "Rank emblem, game you're playing".into()),
            ("r5.emblem".into(), "R5 rank emblem".into()),
            ("ql.emblem".into(), "QL rank emblem".into()),
        ],
        _ => {
            let mut v = vec![
                none,
                ("custom".into(), "Custom text".into()),
                ("playing.summary".into(), "Summary line, game you're playing".into()),
            ];
            v.extend(per_game(&LINES));
            v.extend(per_game(&STATS));
            v
        }
    }
}

// ------------------------------------------------------------------ formatting
fn signed_tenths(d: f64) -> String {
    if d.abs() < 0.05 { "±0".into() } else { format!("{d:+.1}") }
}
fn signed(d: i64) -> String {
    match d {
        0 => "±0".into(),
        d if d > 0 => format!("+{}", thousands(d)),
        d => format!("-{}", thousands(-d)),
    }
}
fn whole(v: f64) -> String {
    thousands(v.round() as i64)
}
fn dash() -> String {
    "–".into()
}

pub fn emblem_url(tier: &str) -> String {
    let name = crate::ranks::TIERS.get(tier_key(tier).max(0) as usize).copied().unwrap_or("nab");
    format!("https://x76.gg/assets/ranks/{name}-256.png")
}

/// "Diamond III · #348 · 1,224 Elo"
pub fn summary(s: &PlayerStats) -> String {
    let mut parts = Vec::new();
    if !s.tier.is_empty() {
        parts.push(s.tier.clone());
    }
    if let Some(r) = s.rank {
        parts.push(format!("#{}", thousands(r)));
    }
    if let Some(e) = s.elo {
        parts.push(format!("{} Elo", whole(e)));
    }
    parts.join(" · ")
}

/// "+12.4 Elo · +2,061 pts · 9W 6L" (last 48 hours)
pub fn recent(s: &PlayerStats) -> String {
    let mut parts = Vec::new();
    if let Some(d) = s.recent_elo {
        parts.push(format!("{} Elo", signed_tenths(d)));
    }
    if let Some(d) = s.recent_points {
        parts.push(format!("{} pts", signed(d)));
    }
    parts.push(format!("{}W {}L", s.recent_wins, s.recent_losses));
    parts.join(" · ")
}

/// One widget line: "48h: 321-244 · +2,061 pts · up from Plat I". Record and points always; then
/// the first of tier change, best win, Elo change that still fits.
pub fn summary_48h(s: &PlayerStats) -> String {
    const FITS: usize = 48;
    let (w, l) = (s.recent_wins, s.recent_losses);
    if w + l == 0 {
        return "48h: no ranked games".into();
    }
    let mut line = format!("48h: {w}-{l}");
    if let Some(d) = s.recent_points {
        line += &format!(" · {} pts", signed(d));
    }
    let mut extras = Vec::new();
    if !s.recent_tier_from.is_empty() && !s.tier.is_empty() && s.recent_tier_from != s.tier {
        let dir = if s.recent_points.unwrap_or(0) >= 0 { "up" } else { "down" };
        extras.push(format!("{dir} from {}", s.recent_tier_from.replace("Platinum", "Plat")));
    }
    if let Some((name, _)) = &s.best_win {
        extras.push(format!("beat {name}"));
    }
    if let Some(d) = s.recent_elo {
        extras.push(format!("{} Elo", signed_tenths(d)));
    }
    if let Some(extra) = extras.into_iter().find(|e| line.chars().count() + 3 + e.chars().count() <= FITS) {
        line += &format!(" · {extra}");
    }
    line
}

/// x76 divisions are 1,000-point bands up to Diamond I; Prism starts at 20,000 and LIGHT is the
/// top of the ladder (its lowest member's points are the bar to clear).
const DIVISIONS: [&str; 20] = [
    "Bronze IV", "Bronze III", "Bronze II", "Bronze I", "Silver IV", "Silver III", "Silver II", "Silver I", "Gold IV", "Gold III",
    "Gold II", "Gold I", "Platinum IV", "Platinum III", "Platinum II", "Platinum I", "Diamond IV", "Diamond III", "Diamond II", "Diamond I",
];
const PRISM_AT: i64 = 20_000;

/// Lowest points among LIGHT players on the last ladder check.
fn light_cutoff(core: &Core, game: Game) -> Option<i64> {
    core.boards.get(&game)?.players.iter().filter(|p| p.tier.eq_ignore_ascii_case("light")).filter_map(|p| p.points).min()
}

/// Progress toward the next tier: (points into this band, band size, next tier). None at the top.
fn tier_goal(s: &PlayerStats, cutoff: Option<i64>) -> Option<(i64, i64, String)> {
    if s.tier.eq_ignore_ascii_case("light") {
        return None;
    }
    let pts = s.points?.max(0);
    if pts < PRISM_AT {
        let band = pts / 1000;
        let next = DIVISIONS.get(band as usize + 1).copied().unwrap_or("Prism");
        return Some((pts - band * 1000, 1000, next.to_string()));
    }
    let top = cutoff.filter(|c| *c > pts).unwrap_or(pts.max(PRISM_AT + 5000));
    Some((pts - PRISM_AT, (top - PRISM_AT).max(1), "LIGHT".into()))
}

/// What a progress bar shows: (current, max, what it means).
pub fn render_progress(core: &Core, cfg: &Config, source: &str) -> Option<(i64, i64, String)> {
    let (game_id, what) = source.split_once('.')?;
    let (game, stats) = subject(core, cfg, game_id)?;
    let s = stats?;
    match what {
        "progress_tier" => Some(match tier_goal(&s, light_cutoff(core, game)) {
            Some((cur, max, next)) => (cur, max, format!("{} / {} to {next}", thousands(cur), thousands(max))),
            None => (1, 1, format!("{} · top of the ladder", s.tier)),
        }),
        "progress_winrate" => {
            let (w, l) = (s.recent_wins, s.recent_losses);
            Some((w, (w + l).max(1), format!("{w} of {} won, last 48h", w + l)))
        }
        _ => None,
    }
}

/// A collection card: (picture URL, name, description).
fn card(s: Option<&PlayerStats>, g: Game, id: &str) -> (String, String, String) {
    let p = short(g);
    let Some(s) = s else {
        return (emblem_url(""), dash(), format!("no {p} player tracked"));
    };
    match id {
        "card_summary" => {
            let rank = s.rank.map(|r| format!(" #{}", thousands(r))).unwrap_or_default();
            let tier = if s.tier.is_empty() { "Unranked".to_string() } else { s.tier.clone() };
            (emblem_url(&s.tier), format!("{tier}{rank}"), summary_48h(s))
        }
        "card_rank" => (
            emblem_url(&s.tier),
            match (s.tier.is_empty(), s.rank) {
                (false, Some(r)) => format!("{} · #{}", s.tier, thousands(r)),
                (false, None) => s.tier.clone(),
                (true, Some(r)) => format!("#{}", thousands(r)),
                (true, None) => "Unranked".into(),
            },
            match (s.elo, s.points) {
                (Some(e), Some(pts)) => format!("{} Elo · {} pts", whole(e), thousands(pts)),
                (Some(e), None) => format!("{} Elo", whole(e)),
                _ => format!("{p} ladder"),
            },
        ),
        "card_change" => {
            let d = s.recent_elo.unwrap_or(0.0);
            let pic = if d >= 0.05 { "Chart increasing" } else if d <= -0.05 { "Chart decreasing" } else { "Bar chart" };
            let pts = s.recent_points.map(|v| format!("{} pts · ", signed(v))).unwrap_or_default();
            (emoji(pic), s.recent_elo.map(|v| format!("{} Elo", signed_tenths(v))).unwrap_or_else(dash), format!("{pts}last 48h"))
        }
        "card_record" => {
            let (w, l) = (s.recent_wins, s.recent_losses);
            // hot streak, rough patch, or even
            let pic = if w > l { "Fire" } else if l > w { "Skull" } else { "Crossed swords" };
            let desc = if w + l == 0 {
                "no games · last 48h".to_string()
            } else {
                format!("{}% won · last 48h", (w as f64 * 100.0 / (w + l) as f64).round() as i64)
            };
            (emoji(pic), format!("{w}W {l}L"), desc)
        }
        "card_best" => match &s.best_win {
            Some((name, e)) => (emoji("Trophy"), format!("{name} · {}", whole(*e)), "best win · last 48h".into()),
            None => (emoji("Trophy"), dash(), "no wins logged · last 48h".into()),
        },
        "card_peak" => (emoji("Glowing star"), s.peak_elo.map(|e| format!("{} Elo", whole(e))).unwrap_or_else(dash), format!("{p} peak since tracking")),
        _ => (String::new(), dash(), String::new()),
    }
}

/// The player and game a source is about ("r5.x", "ql.x" or "playing.x").
fn subject(core: &Core, cfg: &Config, game_id: &str) -> Option<(Game, Option<PlayerStats>)> {
    let game = match game_id {
        "playing" => core.discord.playing.unwrap_or_else(|| {
            if cfg.player(core, Game::R5).is_some() || cfg.player(core, Game::Ql).is_none() { Game::R5 } else { Game::Ql }
        }),
        other => Game::from_id(other)?,
    };
    Some((game, cfg.player(core, game).and_then(|id| player_stats(core, game, &id))))
}

/// What a card source shows: (picture URL, name, description).
pub fn render_card(core: &Core, cfg: &Config, source: &str) -> Option<(String, String, String)> {
    let (game_id, what) = source.split_once('.')?;
    let (game, stats) = subject(core, cfg, game_id)?;
    Some(card(stats.as_ref(), game, what))
}

/// A stat as (value, label), e.g. ("1,224", "R5 Elo").
fn stat(s: Option<&PlayerStats>, g: Game, id: &str) -> (String, String) {
    let p = short(g);
    let Some(s) = s else {
        let label = STATS.iter().find(|(k, _)| *k == id).map(|(_, m)| format!("{p} {m}")).unwrap_or_default();
        return (dash(), label);
    };
    match id {
        "elo" => (s.elo.map(whole).unwrap_or_else(dash), format!("{p} Elo")),
        "points" => (s.points.map(thousands).unwrap_or_else(dash), format!("{p} points")),
        "rank" => (s.rank.map(|r| format!("#{}", thousands(r))).unwrap_or_else(dash), format!("{p} rank")),
        "tier" => (if s.tier.is_empty() { "Unranked".into() } else { s.tier.clone() }, format!("{p} tier")),
        "elo_48h" => (s.recent_elo.map(signed_tenths).unwrap_or_else(dash), format!("{p} Elo, 48h")),
        "points_48h" => (s.recent_points.map(signed).unwrap_or_else(dash), format!("{p} points, 48h")),
        "record_48h" => (format!("{}W {}L", s.recent_wins, s.recent_losses), format!("{p} record, 48h")),
        "kd" => (
            if s.deaths > 0 { format!("{:.2}", s.kills as f64 / s.deaths as f64) } else { dash() },
            format!("{p} K/D"),
        ),
        "peak" => (s.peak_elo.map(whole).unwrap_or_else(dash), format!("{p} peak Elo")),
        "best_win" => match &s.best_win {
            Some((name, e)) => (format!("{} Elo", whole(*e)), format!("{p} best win · {name}")),
            None => (dash(), format!("{p} best win, 48h")),
        },
        _ => (dash(), String::new()),
    }
}

/// What a source shows: (value, label). Text fields use the value, or "label: value" for stats.
pub fn render(core: &Core, cfg: &Config, source: &str) -> Option<(String, String)> {
    let (game_id, what) = source.split_once('.')?;
    let (game, stats) = subject(core, cfg, game_id)?;
    let s = stats.as_ref();
    Some(match what {
        "summary" => (s.map(summary).filter(|t| !t.is_empty()).map(|t| format!("{} · {t}", short(game))).unwrap_or_else(dash), String::new()),
        "recent_line" => (s.map(|s| format!("{} last 48h: {}", short(game), recent(s))).unwrap_or_else(dash), String::new()),
        "summary_48h" => (s.map(summary_48h).unwrap_or_else(dash), String::new()),
        "emblem" => (emblem_url(s.map(|s| s.tier.as_str()).unwrap_or("")), s.map(|s| s.tier.clone()).unwrap_or_default()),
        id => stat(s, game, id),
    })
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max - 1).collect::<String>() + "…" }
}

/// What one slot will show, for the settings preview.
pub fn preview(core: &Core, cfg: &Config, slot: &Slot) -> String {
    let source = cfg.source_of(slot);
    if slot.kind == "progress" && source != "none" {
        return render_progress(core, cfg, &source).map(|(_, _, what)| what).unwrap_or_default();
    }
    if slot.kind == "card" && source != "none" {
        return render_card(core, cfg, &source).map(|(_, name, desc)| format!("{name}  —  {desc}")).unwrap_or_default();
    }
    match source.as_str() {
        "none" => "Discord shows its fallback for this field.".into(),
        "custom" => String::new(),
        _ => match (slot.kind.as_str(), render(core, cfg, &source)) {
            ("tile", Some((v, l))) => format!("{v}  ·  {l}"),
            ("image", Some((_, tier))) => format!("{} emblem from x76.gg", if tier.is_empty() { "Unranked" } else { &tier }),
            (_, Some((v, l))) if !l.is_empty() => format!("{l}: {v}"),
            (_, Some((v, _))) => v,
            _ => String::new(),
        },
    }
}

/// The User Data values to send, in Discord's format.
pub fn payload(core: &Core, cfg: &Config) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut used = std::collections::HashSet::new();
    let mut put = |out: &mut Vec<Value>, key: &str, v: Value| {
        if !key.is_empty() && used.insert(key.to_string()) {
            out.push(v);
        }
    };
    for slot in &cfg.layout {
        let source = cfg.source_of(slot);
        if source == "none" {
            continue;
        }
        if slot.kind == "progress" {
            if let Some((cur, max, _)) = render_progress(core, cfg, &source) {
                put(&mut out, &slot.key, json!({"type": 2, "name": slot.key, "value": cur}));
                put(&mut out, &slot.max_key, json!({"type": 2, "name": slot.max_key, "value": max}));
            }
            continue;
        }
        if slot.kind == "card" {
            if let Some((pic, name, desc)) = render_card(core, cfg, &source) {
                put(&mut out, &slot.name_key, json!({"type": 1, "name": slot.name_key, "value": clip(&name, 64)}));
                put(&mut out, &slot.desc_key, json!({"type": 1, "name": slot.desc_key, "value": clip(&desc, 128)}));
                if !pic.is_empty() {
                    put(&mut out, &slot.image_key, json!({"type": 3, "name": slot.image_key, "value": {"url": pic}}));
                }
            }
            continue;
        }
        if source == "custom" {
            let text = cfg.custom_of(slot);
            if !text.trim().is_empty() && slot.kind != "image" {
                put(&mut out, &slot.key, json!({"type": 1, "name": slot.key, "value": clip(text.trim(), 128)}));
            }
            continue;
        }
        let Some((value, label)) = render(core, cfg, &source) else { continue };
        match slot.kind.as_str() {
            "tile" => {
                put(&mut out, &slot.key, json!({"type": 1, "name": slot.key, "value": clip(&value, 64)}));
                put(&mut out, &slot.label_key, json!({"type": 1, "name": slot.label_key, "value": clip(&label, 64)}));
            }
            "image" => put(&mut out, &slot.key, json!({"type": 3, "name": slot.key, "value": {"url": value}})),
            _ => {
                let text = if label.is_empty() { value } else { format!("{label}: {value}") };
                put(&mut out, &slot.key, json!({"type": 1, "name": slot.key, "value": clip(&text, 128)}));
            }
        }
    }
    out
}

// ------------------------------------------------------------------ Discord's HTTP API
fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .user_agent(BOT_AGENT)
        .build()
        .into()
}

fn answer(result: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value, String> {
    let mut resp = result.map_err(|e| format!("Couldn't reach Discord: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    let detail = || {
        serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| body.chars().take(160).collect())
    };
    match status {
        200..=299 => Ok(serde_json::from_str(&body).unwrap_or(Value::Null)),
        401 => Err("Discord didn't accept the bot token. Reset it in the Developer Portal (your app → Bot) and paste the new one.".into()),
        403 => Err(format!(
            "Discord refused (403: {}). The widget only updates for an account that authorised this app with the sdk.social_layer scope.",
            detail()
        )),
        404 => Err(format!("Discord doesn't know that app or user (404: {}). Check both IDs.", detail())),
        429 => Err("Discord asked x76tracker to slow down. It'll try again in a few minutes.".into()),
        _ => Err(format!("Discord returned {status}: {}", detail())),
    }
}

/// Sends the User Data values to the widget.
pub fn push(token: &str, cfg: &Config, dynamic: &[Value]) -> Result<(), String> {
    let url = format!("{API}/applications/{}/users/{}/identities/0/profile", cfg.app_id, cfg.user_id);
    answer(agent().patch(&url).header("Authorization", &format!("Bot {token}")).send_json(json!({"data": {"dynamic": dynamic}}))).map(|_| ())
}

/// Reads the app's widget layout: (widget name, User Data slots, fixed texts).
pub fn read_layout(token: &str, app_id: &str) -> Result<(String, Vec<Slot>, Vec<StaticField>), String> {
    let url = format!("{API}/applications/{app_id}/widget-configs");
    let v = answer(agent().get(&url).header("Authorization", &format!("Bot {token}")).call())?;
    let list = v.as_array().ok_or("Discord sent an unexpected answer for the widget layout.")?;
    let cfg = list
        .iter()
        .find(|c| c.get("status").and_then(Value::as_str) == Some("published"))
        .or(list.first())
        .ok_or("This app has no widget. Discord doesn't let new apps make one any more.")?;
    let name = cfg.get("display_name").and_then(Value::as_str).unwrap_or("").to_string();
    let (slots, statics) = parse_layout(cfg);
    Ok((name, slots, statics))
}

const SURFACES: [(&str, &str); 5] = [
    ("widget_top", "Top"),
    ("widget_bottom", "Bottom"),
    ("mini_profile", "Mini profile"),
    ("activity_accessory", "While playing"),
    ("add_widget_preview", "Preview"),
];

/// Editor order: title, then subtitles, then the rest; "stat_10" after "stat_9".
fn natural(s: &str) -> (u8, String, u64) {
    let digits: String = s.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    let first = if s == "title" { 0 } else if s.starts_with("subtitle") { 1 } else { 2 };
    (first, s[..s.len() - digits.len()].to_string(), digits.parse().unwrap_or(0))
}

/// "subtitle_1" → "Subtitle 1", "contained_image" → "Image"
fn pretty(component: &str) -> String {
    let c = component.strip_prefix("contained_").unwrap_or(component).replace('_', " ");
    let mut ch = c.trim().chars();
    ch.next().map(|f| f.to_uppercase().collect::<String>() + ch.as_str()).unwrap_or_default()
}

fn parse_layout(cfg: &Value) -> (Vec<Slot>, Vec<StaticField>) {
    let empty = Map::new();
    let surfaces = cfg.get("surfaces").and_then(Value::as_object).unwrap_or(&empty);
    let mut names: Vec<&String> = surfaces.keys().collect();
    names.sort_by_key(|n| (SURFACES.iter().position(|(k, _)| k == n).unwrap_or(99), n.to_string()));
    let (mut slots, mut statics) = (Vec::new(), Vec::new());
    for sname in names {
        let surface = SURFACES.iter().find(|(k, _)| k == sname).map(|(_, l)| l.to_string()).unwrap_or_else(|| pretty(sname));
        let comps = surfaces[sname].get("components").and_then(Value::as_object).unwrap_or(&empty);
        let mut cnames: Vec<&String> = comps.keys().collect();
        cnames.sort_by_key(|c| natural(c));
        for cname in cnames {
            let fields = comps[cname].get("fields").and_then(Value::as_object).unwrap_or(&empty);
            let data_key = |f: &str| {
                fields
                    .get(f)
                    .filter(|v| v.get("value_type").and_then(Value::as_str) == Some("data"))
                    .and_then(|v| v.get("value").and_then(Value::as_str))
                    .map(str::to_string)
            };
            let fallback = |f: &Value| f.pointer("/fallback/value").and_then(Value::as_str).unwrap_or("").to_string();
            if let (Some(key), Some(label_key)) = (data_key("value"), data_key("label")) {
                slots.push(Slot {
                    path: format!("{sname}/{cname}"),
                    kind: "tile".into(),
                    surface: surface.clone(),
                    name: pretty(cname),
                    key,
                    label_key,
                    fallback: fields.get("value").map(fallback).unwrap_or_default(),
                    ..Slot::default()
                });
                continue;
            }
            // a collection item (picture, name, description): one card fills all its User Data parts
            let is_item = fields.contains_key("name") && (fields.contains_key("description") || fields.contains_key("image"));
            // a progress bar: a current value and a max value
            let data_fields: Vec<&String> = fields
                .iter()
                .filter(|(_, v)| v.get("value_type").and_then(Value::as_str) == Some("data"))
                .map(|(k, _)| k)
                .collect();
            let max_field = data_fields.iter().find(|f| f.contains("max")).copied();
            let cur_field = data_fields
                .iter()
                .find(|f| f.contains("current"))
                .or_else(|| data_fields.iter().find(|f| !f.contains("max") && !matches!(f.as_str(), "name" | "description" | "image" | "label")))
                .copied();
            if !is_item && (max_field.is_some() || cname.contains("progress")) {
                if let Some(cur) = cur_field {
                    slots.push(Slot {
                        path: format!("{sname}/{cname}"),
                        kind: "progress".into(),
                        surface: surface.clone(),
                        name: pretty(cname),
                        key: data_key(cur).unwrap_or_default(),
                        max_key: max_field.and_then(|m| data_key(m)).unwrap_or_default(),
                        ..Slot::default()
                    });
                    continue;
                }
            }
            let (n, d, i) = (data_key("name"), data_key("description"), data_key("image"));
            let mut card_parts = Vec::new();
            if is_item && (n.is_some() || d.is_some() || i.is_some()) {
                slots.push(Slot {
                    path: format!("{sname}/{cname}"),
                    kind: "card".into(),
                    surface: surface.clone(),
                    name: pretty(cname),
                    key: n.clone().or_else(|| d.clone()).or_else(|| i.clone()).unwrap_or_default(),
                    name_key: n.unwrap_or_default(),
                    desc_key: d.unwrap_or_default(),
                    image_key: i.unwrap_or_default(),
                    ..Slot::default()
                });
                card_parts = vec!["name", "description", "image"];
            }
            let mut fnames: Vec<&String> = fields.keys().collect();
            fnames.sort();
            for fname in fnames {
                let in_card = card_parts.contains(&fname.as_str())
                    && fields[fname].get("value_type").and_then(Value::as_str) == Some("data");
                if in_card {
                    continue;
                }
                let f = &fields[fname];
                let name = if fields.len() == 1 || matches!(fname.as_str(), "text" | "image") {
                    pretty(cname)
                } else {
                    format!("{} {}", pretty(cname), fname)
                };
                let path = format!("{sname}/{cname}/{fname}");
                let is_image = f.get("presentation_type").and_then(Value::as_str) == Some("image");
                match f.get("value_type").and_then(Value::as_str) {
                    Some("data") => {
                        if let Some(key) = f.get("value").and_then(Value::as_str) {
                            slots.push(Slot {
                                path,
                                kind: (if is_image { "image" } else { "text" }).into(),
                                surface: surface.clone(),
                                name,
                                key: key.into(),
                                fallback: fallback(f),
                                ..Slot::default()
                            });
                        }
                    }
                    Some("custom_string") if !is_image => statics.push(StaticField {
                        path,
                        surface: surface.clone(),
                        name,
                        text: f.get("value").and_then(Value::as_str).unwrap_or("").to_string(),
                    }),
                    _ => {}
                }
            }
        }
    }
    (slots, statics)
}

// ------------------------------------------------------------------ the update loop step
/// Sends new widget values when they changed (at most every 5 minutes), or soon after a settings
/// change or "Update now". Returns true if it tried, so the UI can show the outcome.
pub fn widget_step(shared: &crate::tracker::Shared) -> bool {
    let (cfg, token, dynamic) = {
        let core = shared.lock();
        let cfg = Config::load(&core);
        let Some(token) = core.discord.token.clone() else { return false };
        if !cfg.widget || !cfg.widget_ready() {
            return false;
        }
        let dynamic = payload(&core, &cfg);
        if dynamic.is_empty() {
            return false;
        }
        let t = now();
        let since = core.discord.sent_at.map_or(f64::INFINITY, |s| t - s);
        let manual = core.discord.soon.is_some_and(|s| t >= s) && since >= MANUAL_GAP;
        let auto = dynamic != core.discord.sent && since >= MIN_GAP;
        if !manual && !auto {
            return false;
        }
        (cfg, token, dynamic)
    };
    let result = push(&token, &cfg, &dynamic);
    let mut core = shared.lock();
    let t = now();
    core.discord.sent_at = Some(t);
    core.discord.soon = None;
    match result {
        Ok(()) => {
            core.discord.sent = dynamic;
            core.discord.ok_at = Some(t);
            core.discord.error = None;
        }
        Err(e) => core.discord.error = Some(e),
    }
    true
}

/// Reads the widget layout and stores it in the settings. Blocking.
pub fn refresh_layout(shared: &crate::tracker::Shared) {
    let (token, app_id) = {
        let mut core = shared.lock();
        let cfg = Config::load(&core);
        let token = core.discord.token.clone();
        let note = if !valid_id(&cfg.app_id) {
            Some("Enter the Application ID first.")
        } else if token.is_none() {
            Some("Paste the bot token first.")
        } else {
            None
        };
        if let Some(n) = note {
            core.discord.layout_note = Some((true, n.into()));
            return;
        }
        core.discord.layout_busy = true;
        (token.unwrap_or_default(), cfg.app_id)
    };
    let result = read_layout(&token, &app_id);
    let mut core = shared.lock();
    core.discord.layout_busy = false;
    match result {
        Ok((name, slots, statics)) => {
            let mut cfg = Config::load(&core);
            for s in &cfg.statics {
                cfg.remembered.insert(s.path.clone(), s.text.clone());
            }
            let n = slots.len();
            cfg.widget_name = name.clone();
            cfg.layout = slots;
            cfg.statics = statics;
            cfg.save(&mut core);
            let what = if name.is_empty() { "the widget".to_string() } else { format!("“{name}”") };
            core.discord.layout_note = Some((
                n == 0,
                if n == 0 {
                    format!("Read {what}, but none of its fields is set to User Data, so there's nothing to fill.")
                } else {
                    format!("Read {what}: {n} field{} to fill.", if n == 1 { "" } else { "s" })
                },
            ));
            core.discord.soon = Some(now());
        }
        Err(e) => core.discord.layout_note = Some((true, e)),
    }
}

// ------------------------------------------------------------------ the token, encrypted on disk
/// "Bot abc.def" or "abc.def" with stray spaces → "abc.def"
pub fn clean_token(t: &str) -> String {
    let t = t.trim().trim_matches('"');
    t.strip_prefix("Bot ").unwrap_or(t).trim().to_string()
}

pub fn load_token(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(TOKEN_FILE)).ok()?;
    String::from_utf8(sys::unprotect(&bytes)?).ok().filter(|t| !t.is_empty())
}

pub fn save_token(dir: &Path, token: &str) -> Result<(), String> {
    let sealed = sys::protect(token.as_bytes()).ok_or("Windows couldn't encrypt the token.")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(TOKEN_FILE), sealed).map_err(|e| e.to_string())
}

pub fn clear_token(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(TOKEN_FILE));
}

/// Windows DPAPI: data only this Windows account can decrypt.
#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    #[repr(C)]
    struct Blob {
        len: u32,
        data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(input: *const Blob, descr: *const u16, entropy: *const Blob, reserved: *mut c_void, prompt: *mut c_void, flags: u32, out: *mut Blob) -> i32;
        fn CryptUnprotectData(input: *const Blob, descr: *mut *mut u16, entropy: *const Blob, reserved: *mut c_void, prompt: *mut c_void, flags: u32, out: *mut Blob) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    const UI_FORBIDDEN: u32 = 0x1;

    fn take(out: Blob) -> Vec<u8> {
        unsafe {
            let v = std::slice::from_raw_parts(out.data, out.len as usize).to_vec();
            LocalFree(out.data as *mut c_void);
            v
        }
    }

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        let input = Blob { len: data.len() as u32, data: data.as_ptr() as *mut u8 };
        let mut out = Blob { len: 0, data: null_mut() };
        let ok = unsafe { CryptProtectData(&input, null(), null(), null_mut(), null_mut(), UI_FORBIDDEN, &mut out) };
        (ok != 0).then(|| take(out))
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        let input = Blob { len: data.len() as u32, data: data.as_ptr() as *mut u8 };
        let mut out = Blob { len: 0, data: null_mut() };
        let ok = unsafe { CryptUnprotectData(&input, null_mut(), null(), null_mut(), null_mut(), UI_FORBIDDEN, &mut out) };
        (ok != 0).then(|| take(out))
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn protect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
    pub fn unprotect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(pretty("subtitle_1"), "Subtitle 1");
        assert_eq!(pretty("contained_image"), "Image");
        assert_eq!(pretty("stat"), "Stat");
        assert!(natural("stat_10") > natural("stat_9"));
        assert_eq!(clean_token(" Bot abc.def "), "abc.def");
    }

    #[test]
    fn layout_of_the_kovaaks_widget() {
        let v: Value = serde_json::from_str(
            r#"{"surfaces":{
              "widget_bottom":{"components":{
                "stat_2":{"fields":{"value":{"value_type":"data","value":"pb2_score"},"label":{"value_type":"data","value":"pb2_name"}}},
                "stat_1":{"fields":{"value":{"value_type":"data","value":"pb1_score"},"label":{"value_type":"data","value":"pb1_name"}}}}},
              "widget_top":{"components":{
                "title":{"fields":{"text":{"value_type":"custom_string","presentation_type":"text","value":"hardstuck"}}},
                "subtitle_1":{"fields":{"text":{"value_type":"data","presentation_type":"text","value":"pb_line","fallback":{"value":"[no pbs yet...]"}}}},
                "contained_image":{"fields":{"image":{"value_type":"application_asset","presentation_type":"image","value":"hard"}}}}}
            }}"#,
        )
        .unwrap();
        let (slots, statics) = parse_layout(&v);
        let keys: Vec<&str> = slots.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["pb_line", "pb1_score", "pb2_score"]);
        assert_eq!(slots[1].label_key, "pb1_name");
        assert_eq!(slots[0].fallback, "[no pbs yet...]");
        assert_eq!(statics.len(), 1);
        assert_eq!(statics[0].name, "Title");
        let mut cfg = Config { layout: slots.clone(), ..Config::default() };
        assert_eq!(cfg.source_of(&slots[1]), "r5.elo");
        assert_eq!(cfg.source_of(&slots[2]), "r5.points");
        assert_eq!(cfg.source_of(&slots[0]), "custom");
        assert_eq!(cfg.custom_of(&slots[0]), "[no pbs yet...]");
        cfg.remembered.insert("widget_top/subtitle_1/text".into(), "icon - @someone".into());
        assert_eq!(cfg.custom_of(&slots[0]), "icon - @someone");
    }

    #[test]
    fn collection_items_become_cards() {
        let v: Value = serde_json::from_str(
            r#"{"surfaces":{"widget_bottom":{"layout":"widget_bottom_collection","components":{
                "item_2":{"fields":{
                    "name":{"value_type":"data","presentation_type":"text","value":"card2_name"},
                    "description":{"value_type":"custom_string","presentation_type":"text","value":"fixed words"},
                    "image":{"value_type":"application_asset","presentation_type":"image","value":"lmao"}}},
                "item_1":{"fields":{
                    "name":{"value_type":"data","presentation_type":"text","value":"card1_name"},
                    "description":{"value_type":"data","presentation_type":"text","value":"card1_desc"},
                    "image":{"value_type":"data","presentation_type":"image","value":"card1_img"}}}}}}}"#,
        )
        .unwrap();
        let (slots, statics) = parse_layout(&v);
        assert_eq!(slots.len(), 2);
        assert_eq!((slots[0].kind.as_str(), slots[0].name.as_str()), ("card", "Item 1"));
        assert_eq!((slots[0].name_key.as_str(), slots[0].desc_key.as_str(), slots[0].image_key.as_str()), ("card1_name", "card1_desc", "card1_img"));
        assert_eq!((slots[1].name_key.as_str(), slots[1].desc_key.as_str(), slots[1].image_key.as_str()), ("card2_name", "", ""));
        assert_eq!(statics.len(), 1); // item 2's fixed description
        let mut cfg = Config { layout: slots.clone(), ..Config::default() };
        assert_eq!(cfg.source_of(&slots[0]), "r5.card_rank");
        assert_eq!(cfg.source_of(&slots[1]), "r5.card_change");
        // picked when card1_name was a plain text field: not a card choice, so the default wins
        cfg.sources.insert("card1_name".into(), "r5.tier".into());
        assert_eq!(cfg.source_of(&slots[0]), "r5.card_rank");
        cfg.sources.insert("card1_name".into(), "r5.card_best".into());
        assert_eq!(cfg.source_of(&slots[0]), "r5.card_best");
        assert_eq!(
            emoji("Chart increasing"),
            "https://raw.githubusercontent.com/microsoft/fluentui-emoji/main/assets/Chart%20increasing/3D/chart_increasing_3d.png"
        );
    }

    #[test]
    fn progress_layout_and_tier_goal() {
        let v: Value = serde_json::from_str(
            r#"{"surfaces":{"widget_bottom":{"components":{
                "objective":{"fields":{
                    "image":{"value_type":"application_asset","presentation_type":"image","value":"fear"},
                    "name":{"value_type":"data","presentation_type":"text","value":"goal_name"},
                    "description":{"value_type":"data","presentation_type":"text","value":"goal_desc"}}},
                "progress":{"fields":{
                    "current_value":{"value_type":"data","presentation_type":"number","value":"goal_now"},
                    "max_value":{"value_type":"data","presentation_type":"number","value":"goal_max"}}}}}}}"#,
        )
        .unwrap();
        let (slots, _) = parse_layout(&v);
        assert_eq!(slots.len(), 2);
        assert_eq!((slots[0].kind.as_str(), slots[0].name_key.as_str(), slots[0].image_key.as_str()), ("card", "goal_name", ""));
        assert_eq!((slots[1].kind.as_str(), slots[1].key.as_str(), slots[1].max_key.as_str()), ("progress", "goal_now", "goal_max"));
        let cfg = Config { layout: slots.clone(), ..Config::default() };
        assert_eq!(cfg.source_of(&slots[0]), "r5.card_summary");
        assert_eq!(cfg.source_of(&slots[1]), "r5.progress_tier");

        let mut s = PlayerStats { points: Some(17_433), tier: "Diamond III".into(), ..PlayerStats::default() };
        assert_eq!(tier_goal(&s, None), Some((433, 1000, "Diamond II".into())));
        s.points = Some(19_950);
        assert_eq!(tier_goal(&s, None).map(|g| g.2), Some("Prism".into()));
        s.points = Some(21_000);
        assert_eq!(tier_goal(&s, Some(24_697)), Some((1000, 4697, "LIGHT".into())));
        s.tier = "LIGHT".into();
        assert_eq!(tier_goal(&s, Some(24_697)), None);

        let s = PlayerStats {
            tier: "Diamond III".into(),
            recent_tier_from: "Platinum I".into(),
            recent_wins: 321,
            recent_losses: 244,
            recent_points: Some(2061),
            best_win: Some(("pr0tatip".into(), 1514.3)),
            ..PlayerStats::default()
        };
        assert_eq!(summary_48h(&s), "48h: 321-244 · +2,061 pts · up from Plat I");
        assert_eq!(summary_48h(&PlayerStats::default()), "48h: no ranked games");
        let quiet = PlayerStats { recent_wins: 9, recent_losses: 6, recent_points: Some(120), best_win: Some(("pr0tatip".into(), 1514.0)), ..PlayerStats::default() };
        assert_eq!(summary_48h(&quiet), "48h: 9-6 · +120 pts · beat pr0tatip");
    }

    #[test]
    fn dpapi_round_trip() {
        if let Some(sealed) = sys::protect(b"abc.def") {
            assert_ne!(sealed, b"abc.def");
            assert_eq!(sys::unprotect(&sealed).as_deref(), Some(&b"abc.def"[..]));
        }
    }
}
