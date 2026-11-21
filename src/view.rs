//! Turns tracker state into what the Slint UI shows: cards, charts, results, status lines.

use crate::tracker::{key_of, now, Core, Favorite, Game, Hit, Player, Snap};
use crate::{ChartData, ChartPoint, EventRow, GameData, PlayerCard, SearchResult, Tick};
use chrono::{DateTime, Local, TimeZone};
use slint::{Model, ModelRc, SharedString, VecModel};
use std::collections::{HashMap, HashSet};

// ------------------------------------------------------------------ periods
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Period {
    Today,
    Day,
    All,
}

impl Period {
    pub fn from_index(i: i32) -> Period {
        match i {
            1 => Period::Day,
            2 => Period::All,
            _ => Period::Today,
        }
    }
    pub fn index(self) -> i32 {
        match self {
            Period::Today => 0,
            Period::Day => 1,
            Period::All => 2,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Period::Today => "today",
            Period::Day => "24h",
            Period::All => "all",
        }
    }
    pub fn from_key(s: &str) -> Period {
        match s {
            "24h" => Period::Day,
            "all" => Period::All,
            _ => Period::Today,
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            Period::Today => "today",
            Period::Day => "last 24h",
            Period::All => "since tracking",
        }
    }
    pub fn start(self) -> f64 {
        match self {
            Period::Today => local_midnight(),
            Period::Day => now() - 86_400.0,
            Period::All => f64::NEG_INFINITY,
        }
    }
}

/// UI-only state that isn't saved: expanded result lists, a pending "stop tracking?" and flashes.
#[derive(Default)]
pub struct UiState {
    pub show_all: HashSet<String>,
    pub confirm: Option<String>,
    pub flash: HashMap<String, i32>,
    /// Last text typed in each game's search box (ql, r5) and a counter to drop stale results.
    pub query: [String; 2],
    pub search_seq: [u64; 2],
}

// ------------------------------------------------------------------ formatting
pub fn thousands(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn one_decimal(v: f64) -> String {
    let tenths = (v.abs() * 10.0).round() as i64;
    let s = format!("{}.{}", thousands(tenths / 10), tenths % 10);
    if v < 0.0 && tenths != 0 { format!("-{s}") } else { s }
}

fn fmt(elo: bool, v: f64) -> String {
    if elo { one_decimal(v) } else { thousands(v.round() as i64) }
}

fn local(t: f64) -> DateTime<Local> {
    Local.timestamp_opt(t as i64, 0).single().unwrap_or_else(Local::now)
}
fn local_midnight() -> f64 {
    let midnight = Local::now().date_naive().and_hms_opt(0, 0, 0).unwrap();
    Local.from_local_datetime(&midnight).earliest().map(|d| d.timestamp() as f64).unwrap_or_else(|| now() - 86_400.0)
}
fn is_today(t: f64) -> bool {
    local(t).date_naive() == Local::now().date_naive()
}
pub fn clock(t: f64) -> String {
    local(t).format("%-I:%M %p").to_string()
}
fn clock_day(t: f64) -> String {
    if is_today(t) { clock(t) } else { local(t).format("%b %-d, %-I:%M %p").to_string() }
}
fn short_when(t: f64) -> String {
    if is_today(t) { clock(t) } else { local(t).format("%b %-d").to_string() }
}
fn span(secs: f64) -> String {
    let m = (secs / 60.0).round() as i64;
    if m < 60 {
        format!("{m} min")
    } else if m < 48 * 60 {
        format!("{}h {}m", m / 60, m % 60)
    } else {
        format!("{} days", (m as f64 / 1440.0).round() as i64)
    }
}
pub fn ago(t: Option<f64>) -> String {
    let Some(t) = t else { return "never".into() };
    let s = (now() - t).max(0.0).round() as i64;
    match s {
        0..=4 => "just now".into(),
        5..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}
fn dir(d: f64, eps: f64) -> i32 {
    if d > eps { 1 } else if d < -eps { -1 } else { 0 }
}
pub fn tier_key(tier: &str) -> i32 {
    match tier.split_whitespace().next().unwrap_or("").to_lowercase().as_str() {
        "bronze" => 1,
        "silver" => 2,
        "gold" => 3,
        "platinum" => 4,
        "diamond" => 5,
        "prism" => 6,
        "light" => 7,
        _ => 0,
    }
}
fn s(v: impl Into<SharedString>) -> SharedString {
    v.into()
}
fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

// ------------------------------------------------------------------ history maths
fn baseline(h: &[Snap], start: f64) -> Option<&Snap> {
    h.iter().take_while(|s| s.t <= start).last().or(h.first())
}

struct Event {
    t: f64,
    w: i64,
    l: i64,
    dp: Option<f64>,
    de: Option<f64>,
    tier_from: String,
    tier_to: String,
}

fn events(h: &[Snap], start: f64) -> Vec<Event> {
    h.windows(2)
        .filter(|w| w[1].t > start)
        .map(|w| {
            let (a, b) = (&w[0], &w[1]);
            Event {
                t: b.t,
                w: (b.kills - a.kills).max(0),
                l: (b.deaths - a.deaths).max(0),
                dp: b.points.zip(a.points).map(|(x, y)| (x - y) as f64),
                de: b.elo.zip(a.elo).map(|(x, y)| x - y),
                tier_from: a.tier.clone(),
                tier_to: b.tier.clone(),
            }
        })
        .collect()
}

/// Delta text without sign (the ▲/▼ marker carries it), "no change" when flat.
fn delta_text(elo: bool, d: Option<f64>, unit: &str, flat: &str) -> (String, i32) {
    match d {
        None => ("–".into(), 0),
        Some(d) => {
            let di = dir(d, if elo { 0.05 } else { 0.5 });
            if di == 0 { (flat.into(), 0) } else { (format!("{}{unit}", fmt(elo, d.abs())), di) }
        }
    }
}

// ------------------------------------------------------------------ charts
fn nice_step(x: f64) -> f64 {
    let mag = 10f64.powf(x.log10().floor());
    let r = x / mag;
    (if r <= 1.0 { 1.0 } else if r <= 2.0 { 2.0 } else if r <= 2.5 { 2.5 } else if r <= 5.0 { 5.0 } else { 10.0 }) * mag
}

fn chart(h: &[Snap], start: f64, elo: bool) -> Option<ChartData> {
    let val = |s: &Snap| if elo { s.elo } else { s.points.map(|p| p as f64) };
    let all: Vec<(f64, f64, &Snap)> = h.iter().filter_map(|s| val(s).map(|v| (s.t, v, s))).collect();
    let before = all.iter().rev().find(|p| p.0 <= start);
    // (t, value, snapshot, carried over from before the period)
    let mut pts: Vec<(f64, f64, &Snap, bool)> = all.iter().filter(|p| p.0 > start).map(|p| (p.0, p.1, p.2, false)).collect();
    if let Some(b) = before {
        pts.insert(0, (start.max(b.0), b.1, b.2, true));
    }
    let t_now = now();
    if pts.is_empty() || (pts.len() < 2 && t_now - pts[0].0 <= 60.0) {
        return None;
    }
    let t0 = pts[0].0;
    let t1 = t_now.max(pts.last().unwrap().0 + 1.0);
    let mut lo = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let mut hi = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let min_span = if elo { 2.0 } else { 20.0 };
    if hi - lo < min_span {
        let c = (hi + lo) / 2.0;
        lo = c - min_span / 2.0;
        hi = c + min_span / 2.0;
    }
    let pad = (hi - lo) * 0.14;
    lo -= pad;
    hi += pad;
    let fx = |t: f64| ((t - t0) / (t1 - t0)) as f32;
    let fy = |v: f64| (1.0 - (v - lo) / (hi - lo)) as f32;

    let step = nice_step((hi - lo) / 3.0);
    let mut ticks = Vec::new();
    let mut v = (lo / step).ceil() * step;
    while v <= hi {
        let label = if elo && step < 1.0 { one_decimal(v) } else { thousands(v.round() as i64) };
        ticks.push(Tick { y: fy(v), label: s(label) });
        v += step;
    }

    let unit = if elo { "" } else { "P" };
    let points: Vec<ChartPoint> = pts
        .iter()
        .enumerate()
        .map(|(i, &(t, v, snap, carried))| {
            let (change, d) = if i == 0 { (String::new(), 0) } else { delta_text(elo, Some(v - pts[i - 1].1), unit, "no change") };
            let tier = snap.tier.to_lowercase();
            ChartPoint {
                x: fx(t),
                y: fy(v),
                value: s(fmt(elo, v)),
                when: s(if carried { format!("value at {}", clock_day(t)) } else { format!("changed {}", clock_day(t)) }.to_uppercase()),
                change: s(change),
                dir: d,
                sub: s(match snap.rank {
                    Some(r) => format!("#{} · {tier}", thousands(r)),
                    None => tier,
                }),
            }
        })
        .collect();
    let lookup: Vec<i32> = (0..200)
        .map(|b| {
            let f = (b as f32 + 0.5) / 200.0;
            points.iter().rposition(|p| p.x <= f).unwrap_or(0) as i32
        })
        .collect();
    let multi_day = t1 - t0 > 86_400.0 || !is_today(t0);
    Some(ChartData {
        ticks: model(ticks),
        points: model(points),
        lookup: model(lookup),
        start: s(if multi_day { local(t0).format("%b %-d, %-I:%M %p").to_string() } else { clock(t0) }),
        span: s(span(t1 - t0)),
    })
}

/// Step line through normalised points (x, y in 0..1, y down) drawn at w x h pixels:
/// each value is held flat until the next change, then jumps. `area` closes it to the bottom.
pub fn step_path(points: &ModelRc<ChartPoint>, w: f32, h: f32, area: bool) -> SharedString {
    let pts: Vec<(f32, f32)> = points.iter().map(|p| (p.x * w, p.y * h)).collect();
    let Some(&(_, y0)) = pts.first() else { return SharedString::default() };
    let mut d = format!("M 0 {y0:.1}");
    for pair in pts.windows(2) {
        let ((_, ya), (xb, yb)) = (pair[0], pair[1]);
        d += &format!(" L {xb:.1} {ya:.1} L {xb:.1} {yb:.1}");
    }
    d += &format!(" L {w:.1} {:.1}", pts.last().unwrap().1);
    if area {
        d += &format!(" L {w:.1} {h:.1} L 0 {h:.1} Z");
    }
    d.into()
}

// ------------------------------------------------------------------ cards
fn metric_is_elo(core: &Core, key: &str) -> bool {
    core.state
        .settings
        .get("metrics")
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        == Some("elo")
}

fn card(core: &Core, game: Game, fav: &Favorite, idx: usize, count: usize, period: Period, ui: &UiState) -> PlayerCard {
    let key = key_of(game.id(), &fav.id);
    let h: &[Snap] = core.state.history.get(&key).map(Vec::as_slice).unwrap_or(&[]);
    let live: Option<&Player> = core.latest.get(&key).and_then(Option::as_ref);
    let missing = core.boards.contains_key(&game) && matches!(core.latest.get(&key), Some(None));
    let last = h.last();

    let rank = live.map(|p| p.rank).or_else(|| last.and_then(|s| s.rank));
    let points = live.and_then(|p| p.points).or_else(|| last.and_then(|s| s.points));
    let elo = live.and_then(|p| p.elo).or_else(|| last.and_then(|s| s.elo));
    let kills = live.map(|p| p.kills).or_else(|| last.map(|s| s.kills)).unwrap_or(0);
    let deaths = live.map(|p| p.deaths).or_else(|| last.map(|s| s.deaths)).unwrap_or(0);
    let tier = live.map(|p| p.tier.clone()).or_else(|| last.map(|s| s.tier.clone())).unwrap_or_default();

    let start = period.start();
    let base = baseline(h, start);
    let evs = events(h, start);
    let (wins, losses) = evs.iter().fold((0, 0), |(w, l), e| (w + e.w, l + e.l));
    let d_pts = base.and_then(|b| points.zip(b.points)).map(|(c, b)| (c - b) as f64);
    let d_elo = base.and_then(|b| elo.zip(b.elo)).map(|(c, b)| c - b);
    let d_rank = base.and_then(|b| rank.zip(b.rank)).map(|(c, b)| b - c).unwrap_or(0);
    let since = match base {
        Some(b) if b.t > start => format!("since {}", if period == Period::All { clock_day(b.t) } else { clock(b.t) }),
        _ => period.word().to_string(),
    };

    let show_elo = metric_is_elo(core, &key);
    let show_all = ui.show_all.contains(&key);
    let chart_data = chart(h, start, show_elo);
    let rows: Vec<EventRow> = evs
        .iter()
        .rev()
        .take(if show_all { 300 } else { 5 })
        .map(|e| {
            let note = if e.tier_from != e.tier_to && !e.tier_to.is_empty() {
                format!("→ {}", e.tier_to.to_lowercase())
            } else if e.w == 0 && e.l == 0 {
                if e.dp.unwrap_or(0.0) < 0.0 && e.de.unwrap_or(0.0) == 0.0 { "decay".into() } else { "adjusted".into() }
            } else {
                String::new()
            };
            let (elo_t, elo_d) = delta_text(true, e.de, "", "–");
            let (pts_t, pts_d) = delta_text(false, e.dp, "P", "–");
            EventRow {
                when: s(short_when(e.t)),
                when_full: s(clock_day(e.t)),
                wins: e.w as i32,
                losses: e.l as i32,
                note: s(note),
                elo: s(elo_t),
                elo_dir: elo_d,
                pts: s(pts_t),
                pts_dir: pts_d,
            }
        })
        .collect();

    let (pd, pdir) = delta_text(false, d_pts, "P", "no change");
    let (ed, edir) = delta_text(true, d_elo, "", "no change");
    let p24 = live.and_then(|p| p.points_24h);
    let pos24 = live.and_then(|p| p.position_24h);
    let (site24, site24_dir) = match p24 {
        Some(0) => ("±0P".to_string(), 0),
        Some(v) => (format!("{}P", thousands(v.abs())), v.signum() as i32),
        None => (String::new(), 0),
    };
    let r = rank.unwrap_or(0);

    PlayerCard {
        game: s(game.id()),
        id: s(fav.id.as_str()),
        key: s(key.as_str()),
        name: s(fav.name.as_str()),
        link: game == Game::Ql,
        rank: s(rank.map(thousands).unwrap_or_else(|| "–".into())),
        top: if (1..=3).contains(&r) { r as i32 } else { 0 },
        tier: s(if tier.is_empty() { "unranked".into() } else { tier.to_lowercase() }),
        tier_key: tier_key(&tier),
        moved: s(d_rank.abs().to_string()),
        moved_dir: d_rank.signum() as i32,
        missing,
        points: s(points.map(thousands).unwrap_or_else(|| "–".into())),
        points_delta: s(pd),
        points_dir: pdir,
        since: s(since),
        elo: s(elo.map(one_decimal).unwrap_or_else(|| "–".into())),
        elo_delta: s(ed),
        elo_dir: edir,
        elo_field: s(game.elo_field()),
        wins: wins as i32,
        losses: losses as i32,
        show_elo,
        show_all,
        confirm_remove: ui.confirm.as_deref() == Some(key.as_str()),
        flash: ui.flash.get(&key).copied().unwrap_or(0),
        has_chart: chart_data.is_some(),
        chart: chart_data.unwrap_or_default(),
        tracking_since: s(h.first().map(|f| clock_day(f.t)).unwrap_or_else(|| "now".into())),
        events: model(rows),
        total_events: evs.len() as i32,
        kd: s(format!("{} / {}", thousands(kills), thousands(deaths))),
        kd_ratio: s(if deaths > 0 { format!("{:.2}", kills as f64 / deaths as f64) } else { "–".into() }),
        acc: s(live.and_then(|p| p.acc).map(|a| format!("{a:.0}%")).unwrap_or_default()),
        site24: s(site24),
        site24_dir,
        site24_places: s(pos24.map(|v| v.abs().to_string()).unwrap_or_default()),
        site24_places_dir: pos24.map(|v| v.signum() as i32).unwrap_or(0),
        first: idx == 0,
        last: idx + 1 == count,
    }
}

pub fn game_data(core: &Core, game: Game, period: Period, ui: &UiState) -> GameData {
    let favs: Vec<&Favorite> = core.state.favorites.iter().filter(|f| f.game == game.id()).collect();
    let cards: Vec<PlayerCard> = favs.iter().enumerate().map(|(i, f)| card(core, game, f, i, favs.len(), period, ui)).collect();
    GameData {
        id: s(game.id()),
        label: s(game.label()),
        mode: s(game.mode()),
        placeholder: s(match game {
            Game::Ql => "Find a player, alias or SteamID",
            Game::R5 => "Find a player by in-game name",
        }),
        players: model(cards),
    }
}

pub fn results(hits: &[Hit]) -> ModelRc<SearchResult> {
    model(
        hits.iter()
            .map(|h| SearchResult {
                id: s(h.player.id.as_str()),
                rank: s(format!("#{}", thousands(h.player.rank))),
                name: s(h.player.name.as_str()),
                tier: s(h.player.tier.to_lowercase()),
                tier_key: tier_key(&h.player.tier),
                points: s(h.player.points.map(thousands).unwrap_or_else(|| "–".into())),
                alias: s(h.alias.as_deref().map(|a| format!("aka {a}")).unwrap_or_default()),
                tracked: h.tracked,
            })
            .collect(),
    )
}

// ------------------------------------------------------------------ status lines
/// Top-bar status: (text, state) where state is 0 idle, 1 live, 2 checking, 3 error.
pub fn live_status(core: &Core) -> (String, i32) {
    let tracked: Vec<Game> = Game::ALL.into_iter().filter(|g| core.state.favorites.iter().any(|f| f.game == g.id())).collect();
    let next = core.next_poll.map(|t| (t - now()).max(0.0).round() as i64);
    let next_txt = next.map(|n| format!(" · next {n}s")).unwrap_or_default();
    if tracked.is_empty() {
        return ("idle · track a player to start".into(), 0);
    }
    if tracked.iter().any(|g| core.status[g].busy) {
        return ("checking the ladder…".into(), 2);
    }
    if tracked.iter().any(|g| core.status[g].failing()) {
        return (format!("can't reach x76{}", next.map(|n| format!(" · retry in {n}s")).unwrap_or_default()), 3);
    }
    let fetched = tracked.iter().filter_map(|g| core.status[g].fetched).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
    (format!("live · {}{next_txt}", ago(fetched)), 1)
}

/// Line under each game's heading: (text, is_error).
pub fn game_status(core: &Core, game: Game) -> (String, bool) {
    let st = &core.status[&game];
    let tracked = core.state.favorites.iter().any(|f| f.game == game.id());
    if st.failing() {
        return (format!("last check failed: {}", st.error.as_deref().unwrap_or("unknown error")), true);
    }
    if st.fetched.is_some() {
        return (format!("{} ranked players · checked {}", thousands(st.count as i64), ago(st.fetched)), false);
    }
    if st.busy || tracked {
        return ("loading ladder…".into(), false);
    }
    ("not checked until you track someone".into(), false)
}
