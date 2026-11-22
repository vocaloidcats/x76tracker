//! Turns tracker state into what the Slint UI shows: cards, charts, results, status lines.

use crate::tracker::{key_of, now, Core, Favorite, Game, Hit, Player, Snap};
use crate::{
    ChangeEntry, ChangeItem, ChangeMedia, ChangeSection, ShareCard, ShareGame, UpdatesPage,
    ChartData, ChartPoint, DiscordPage, EventDetail, EventRow, FightDetail, GameData, GameRank, Opponent, PlayerCard, ProfileCard, SearchResult,
    RecapLine, RecapPage, RivalFight, RivalPage, RivalTile,
    ServerGame, ServerRow, ServersPage, StatChange, StaticText, Tick, WidgetSlot,
};
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
    /// The result whose detail panel is open: (card key, row id).
    pub detail: Option<(String, String)>,
    /// The opponent whose head-to-head panel is open: (card key, opponent's name).
    pub rival: Option<(String, String)>,
    /// The session whose recap is open (index into Core::sessions).
    pub recap: Option<usize>,
    /// Row ids already on each card's board (new ones fade in) and the points/Elo last shown (they tick).
    pub seen: HashMap<String, HashSet<String>>,
    pub shown_nums: HashMap<String, (f64, f64)>,
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
    t_prev: f64,
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
                t_prev: a.t,
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
/// A tracked player's fights from the game's own log: R5Reloaded by in-game name, Quake Live by
/// SteamID (the console log's account folder).
fn fights_for<'a>(core: &'a Core, game: Game, id: &str) -> Vec<&'a crate::r5log::Fight> {
    match game {
        Game::R5 => core.fights.iter().filter(|f| f.me.eq_ignore_ascii_case(id)).collect(),
        Game::Ql => core.ql_fights.iter().filter(|f| f.me == id).collect(),
    }
}

/// "47%" for an accuracy.
fn pct(v: f64) -> String {
    format!("{v:.0}%")
}

/// One opponent as shown in a fight entry, with their rating before the fight.
fn opponent_of(f: &crate::r5log::Fight) -> Opponent {
    let won = f.kind == "kill";
    let gap = f.opp_elo.zip(f.my_elo).map(|(o, m)| o - m);
    let (gap_t, gap_d) = match gap {
        Some(g) if g.abs() >= 0.5 => (thousands(g.abs().round() as i64), if g > 0.0 { 1 } else { -1 }),
        _ => (String::new(), 0),
    };
    let chance = f.my_elo.zip(f.opp_elo).map(|(m, o)| win_chance(m, o));
    Opponent {
        name: s(if f.opponent.is_empty() { "?" } else { f.opponent.as_str() }),
        elo: s(f.opp_elo.map(|v| thousands(v.round() as i64)).unwrap_or_default()),
        tier_key: f.opp_tier.as_deref().map(tier_key).unwrap_or(-1),
        gap: s(gap_t),
        gap_dir: gap_d,
        won,
        upset: chance.is_some_and(|c| (won && c < 0.35) || (!won && c > 0.65)),
    }
}

/// x76 books points in batches every few minutes, a little after the Elo. From this player's own
/// history: when the next booking is due, and roughly how much it'll be (points ≈ a + b × Elo
/// gained since the last booking). Returns (expected time, expected points).
fn predict_points(h: &[Snap]) -> Option<(f64, f64)> {
    const LAG: f64 = 30.0; // a booking covers Elo up to about 30 s before it
    let steps: Vec<(f64, f64, i64)> = h
        .windows(2)
        .map(|w| (w[1].t, w[1].elo.zip(w[0].elo).map(|(b, a)| b - a).unwrap_or(0.0), w[1].points.zip(w[0].points).map(|(b, a)| b - a).unwrap_or(0)))
        .collect();
    let bookings: Vec<(f64, i64)> = steps.iter().filter(|x| x.2 != 0).map(|x| (x.0, x.2)).collect();
    let &(last, _) = bookings.last()?;
    let elo_between = |lo: f64, hi: f64| steps.iter().filter(|x| x.0 > lo && x.0 <= hi).map(|x| x.1).sum::<f64>();
    let pending = elo_between(last - LAG, now());
    if pending.abs() < 0.05 {
        return None; // nothing has moved since x76 last booked points
    }
    // fit from this player's past bookings once there are enough of them
    let samples: Vec<(f64, f64)> = bookings.windows(2).map(|w| (elo_between(w[0].0 - LAG, w[1].0 - LAG), w[1].1 as f64)).collect();
    let (a, b) = if samples.len() >= 8 {
        let n = samples.len() as f64;
        let (mx, my) = (samples.iter().map(|v| v.0).sum::<f64>() / n, samples.iter().map(|v| v.1).sum::<f64>() / n);
        let sxx: f64 = samples.iter().map(|v| (v.0 - mx).powi(2)).sum();
        if sxx > 0.0 {
            let b = samples.iter().map(|v| (v.0 - mx) * (v.1 - my)).sum::<f64>() / sxx;
            (my - b * mx, b)
        } else {
            (0.0, 6.7)
        }
    } else {
        (0.0, 6.7)
    };
    let mut gaps: Vec<f64> = bookings.windows(2).map(|w| w[1].0 - w[0].0).filter(|g| *g > 60.0 && *g < 3600.0).collect();
    gaps.sort_by(f64::total_cmp);
    let cadence = if gaps.len() >= 3 { gaps[gaps.len() / 2] } else { 300.0 }.clamp(120.0, 1800.0);
    let mut when = last + cadence;
    while when < now() {
        when += cadence;
    }
    Some((when, a + b * pending))
}

/// The results list. Where the game log has fights, each fight is its own entry (when it
/// happened). x76's later bookings: an Elo change booked right after a single fight is shown on
/// that fight; otherwise Elo gets its own entry, and points always do. Elsewhere it's the
/// ladder's own changes with W/L from its counts. Newest first, plus a predicted next booking.
fn timeline(h: &[Snap], evs: &[Event], mine: &[&crate::r5log::Fight], start: f64, show_all: bool, interval: f64) -> (Vec<EventRow>, usize) {
    // how soon after a fight an Elo booking still counts as that fight's
    let close = (2.0 * interval).max(120.0);
    let blank = |kind: i32, t: f64| EventRow {
        kind,
        id: s(row_id(t)),
        when: s(short_when(t)),
        when_full: s(clock_day(t)),
        opponents: model(Vec::<Opponent>::new()),
        ..Default::default()
    };
    let mut items: Vec<(f64, EventRow)> = Vec::new();
    // Elo (text, direction) and tier notes that belong on a fight row, by index into `mine`
    let mut on_fight: HashMap<usize, (String, i32)> = HashMap::new();
    let mut fight_note: HashMap<usize, String> = HashMap::new();
    for e in evs {
        let in_window: Vec<usize> = (0..mine.len()).filter(|&i| mine[i].t > e.t_prev && mine[i].t <= e.t).collect();
        let (elo_t, elo_d) = delta_text(true, e.de, "", "–");
        let (pts_t, pts_d) = delta_text(false, e.dp, "P", "–");
        let tier_note = if e.tier_from != e.tier_to && !e.tier_to.is_empty() { format!("→ {}", e.tier_to.to_lowercase()) } else { String::new() };
        if in_window.is_empty() {
            let mut row = blank(0, e.t);
            row.elo = s(elo_t);
            row.elo_dir = elo_d;
            row.pts = s(pts_t);
            row.pts_dir = pts_d;
            row.wins = e.w as i32;
            row.losses = e.l as i32;
            row.note = s(if !tier_note.is_empty() {
                tier_note
            } else if e.w == 0 && e.l == 0 {
                if e.dp.unwrap_or(0.0) < 0.0 && e.de.unwrap_or(0.0) == 0.0 { "decay".into() } else { "adjusted".into() }
            } else {
                String::new()
            });
            items.push((e.t, row));
            continue;
        }
        let moved_e = e.de.is_some_and(|d| d.abs() >= 0.005);
        let moved_p = e.dp.is_some_and(|d| d != 0.0);
        let only = in_window[0];
        let attach = moved_e && in_window.len() == 1 && e.t - mine[only].t <= close;
        if attach {
            on_fight.insert(only, (elo_t.clone(), elo_d));
        }
        let mut placed_note = false;
        // points: always their own entry
        if moved_p {
            let mut row = blank(2, e.t);
            row.label = s("x76 booked points");
            row.pts = s(pts_t);
            row.pts_dir = pts_d;
            row.note = s(tier_note.as_str());
            placed_note = true;
            items.push((e.t, row));
        }
        // Elo that couldn't be put on a single fight
        if moved_e && !attach {
            let mut row = blank(2, e.t);
            row.label = s(if in_window.len() > 1 { format!("x76 booked elo ({} games)", in_window.len()) } else { "x76 booked elo".to_string() });
            row.elo = s(elo_t);
            row.elo_dir = elo_d;
            if !placed_note {
                row.note = s(tier_note.as_str());
                placed_note = true;
            }
            items.push((e.t, row));
        }
        if !placed_note && !tier_note.is_empty() {
            fight_note.insert(only, tier_note);
        }
    }
    for (i, f) in mine.iter().enumerate().filter(|(_, f)| f.t > start) {
        let mut row = blank(1, f.t);
        // clicking a fight opens the ladder change it falls in
        row.id = s(h.iter().find(|snap| snap.t >= f.t).map(|snap| row_id(snap.t)).unwrap_or_default());
        if f.kind == "kill" {
            row.wins = 1;
        } else {
            row.losses = 1;
        }
        row.opponents = model(vec![opponent_of(f)]);
        if let Some((t, d)) = on_fight.get(&i) {
            row.elo = s(t.as_str());
            row.elo_dir = *d;
        }
        if let Some(n) = fight_note.get(&i) {
            row.note = s(n.as_str());
        }
        items.push((f.t, row));
    }
    items.sort_by(|a, b| b.0.total_cmp(&a.0));
    let total = items.len();
    let mut rows: Vec<EventRow> = items.into_iter().take(if show_all { 300 } else { 5 }).map(|(_, r)| r).collect();
    if let Some((when, amount)) = predict_points(h) {
        let mut row = blank(3, when);
        row.id = s("");
        row.when = s(format!("~{}", clock(when)));
        row.label = s("next points update (estimate)");
        let d = if amount > 0.5 { 1 } else if amount < -0.5 { -1 } else { 0 };
        row.pts = s(format!("~{}P", thousands(amount.abs().round() as i64)));
        row.pts_dir = d;
        rows.insert(0, row);
    }
    (rows, total)
}

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

    let mine = fights_for(core, game, &fav.id);
    // Quake Live: your accuracy over this period's fights (the ladder has none)
    let fight_acc: Vec<f64> = mine.iter().filter(|f| f.t >= start).filter_map(|f| f.my_acc).collect();
    let in_period: Vec<&crate::r5log::Fight> = mine.iter().copied().filter(|f| f.t >= start).collect();
    let (nemesis, victim) = rivals_of(&in_period);
    let recap = if crate::discord::Config::load(core).player(core, game).as_deref() == Some(fav.id.as_str()) {
        core.sessions.iter().rposition(|x| x.game == game.id()).map(|i| i as i32).unwrap_or(-1)
    } else {
        -1
    };
    let show_elo = metric_is_elo(core, &key);
    let show_all = ui.show_all.contains(&key);
    let chart_data = chart(h, start, show_elo);
    let (mut rows, total_rows) = timeline(h, &evs, &mine, start, show_all, core.interval() as f64);
    // results that weren't on this board last time fade in (nothing does on the first draw)
    if let Some(seen) = ui.seen.get(&key) {
        for r in rows.iter_mut() {
            r.fresh = !r.id.is_empty() && !seen.contains(r.id.as_str());
        }
    }

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

    let (points_from, elo_from) = ui.shown_nums.get(&key).copied().unwrap_or((points.unwrap_or(0) as f64, elo.unwrap_or(0.0)));
    PlayerCard {
        has_num: points.is_some() && elo.is_some(),
        points_num: points.unwrap_or(0) as f32,
        points_from: points_from as f32,
        elo_num: elo.unwrap_or(0.0) as f32,
        elo_from: elo_from as f32,
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
        total_events: total_rows as i32,
        kd: s(format!("{} / {}", thousands(kills), thousands(deaths))),
        kd_ratio: s(if deaths > 0 { format!("{:.2}", kills as f64 / deaths as f64) } else { "–".into() }),
        acc: s(live.and_then(|p| p.acc).map(pct).unwrap_or_else(|| {
            if fight_acc.is_empty() {
                String::new()
            } else {
                let n = fight_acc.len();
                format!("{} · {n} fight{}", pct(fight_acc.iter().sum::<f64>() / n as f64), if n == 1 { "" } else { "s" })
            }
        })),
        site24: s(site24),
        site24_dir,
        site24_places: s(pos24.map(|v| v.abs().to_string()).unwrap_or_default()),
        site24_places_dir: pos24.map(|v| v.signum() as i32).unwrap_or(0),
        first: idx == 0,
        last: idx + 1 == count,
        nemesis: s(nemesis.as_ref().map(|r| r.name.as_str()).unwrap_or_default()),
        nemesis_rec: s(nemesis.as_ref().map(|r| format!("{}–{}", r.wins, r.losses)).unwrap_or_default()),
        victim: s(victim.as_ref().map(|r| r.name.as_str()).unwrap_or_default()),
        victim_rec: s(victim.as_ref().map(|r| format!("{}–{}", r.wins, r.losses)).unwrap_or_default()),
        recap,
    }
}

// ------------------------------------------------------------------ session recap
/// The recap of one game session, for the player picked as yours for that game: fights from the
/// game log, Elo, points and rank from the ladder checks before and after (x76 books them a few
/// minutes late, so the recap keeps filling in for a while).
pub fn recap_page(core: &Core, idx: usize) -> Option<RecapPage> {
    const LAG: f64 = 900.0;
    let sess = core.sessions.get(idx)?;
    let game = Game::from_id(&sess.game)?;
    let id = crate::discord::Config::load(core).player(core, game)?;
    let key = key_of(game.id(), &id);
    let fav = core.state.favorites.iter().find(|f| f.game == game.id() && f.id == id)?;
    let h: &[Snap] = core.state.history.get(&key).map(Vec::as_slice).unwrap_or(&[]);
    // the ladder after: bookings up to LAG after the end, but not into this game's next session
    let next_start = core.sessions.iter().filter(|x| x.game == sess.game && x.start > sess.start).map(|x| x.start).fold(f64::INFINITY, f64::min);
    let until = (sess.end + LAG).min(now()).min(next_start);
    let before = h.iter().take_while(|x| x.t <= sess.start).last();
    let after = h.iter().take_while(|x| x.t <= until).last().filter(|a| before.map_or(true, |b| a.t > b.t));
    let fights: Vec<&crate::r5log::Fight> = fights_for(core, game, &id).into_iter().filter(|f| f.t >= sess.start && f.t <= sess.end + 5.0).collect();
    let (wins, losses) = if !fights.is_empty() {
        let w = fights.iter().filter(|f| f.kind == "kill").count() as i64;
        (w, fights.len() as i64 - w)
    } else {
        before.zip(after).map(|(a, b)| ((b.kills - a.kills).max(0), (b.deaths - a.deaths).max(0))).unwrap_or((0, 0))
    };

    let mut tiles = Vec::new();
    let signed = |v: f64, text: String| if v > 0.0 { format!("+{text}") } else if v < 0.0 { format!("−{text}") } else { text };
    if let (Some(a), Some(b)) = (before.and_then(|x| x.points), after.and_then(|x| x.points)) {
        let d = (b - a) as f64;
        tiles.push(RivalTile {
            label: s("POINTS"),
            value: s(signed(d, format!("{}P", thousands((b - a).abs())))),
            sub: s(format!("{}P → {}P", thousands(a), thousands(b))),
            dir: d.signum() as i32,
        });
    }
    if let (Some(a), Some(b)) = (before.and_then(|x| x.elo), after.and_then(|x| x.elo)) {
        let d = b - a;
        tiles.push(RivalTile {
            label: s("ELO"),
            value: s(signed(if d.abs() < 0.005 { 0.0 } else { d }, two_decimals(d.abs()))),
            sub: s(format!("{} → {}", two_decimals(a), two_decimals(b))),
            dir: if d.abs() < 0.005 { 0 } else { d.signum() as i32 },
        });
    }
    if let (Some(a), Some(b)) = (before.and_then(|x| x.rank), after.and_then(|x| x.rank)) {
        let up = a - b; // a lower number is better
        tiles.push(RivalTile {
            label: s("RANK"),
            value: s(if up == 0 { "same".to_string() } else { format!("{}{}", if up > 0 { "+" } else { "−" }, thousands(up.abs())) }),
            sub: s(format!("place{} · #{} → #{}", if up.abs() == 1 { "" } else { "s" }, thousands(a), thousands(b))),
            dir: up.signum() as i32,
        });
    }
    let accs: Vec<(f64, Option<f64>)> = fights.iter().filter_map(|f| f.my_acc.map(|m| (m, f.opp_acc))).collect();
    if !accs.is_empty() {
        let k = accs.len() as f64;
        let mine = accs.iter().map(|a| a.0).sum::<f64>() / k;
        let theirs: Vec<f64> = accs.iter().filter_map(|a| a.1).collect();
        let sub = if theirs.is_empty() {
            format!("{} fights", accs.len())
        } else {
            format!("opponents {} · {} fights", pct(theirs.iter().sum::<f64>() / theirs.len() as f64), accs.len())
        };
        tiles.push(RivalTile { label: s("ACCURACY"), value: s(pct(mine)), sub: s(sub), dir: 0 });
    } else if wins + losses > 0 {
        tiles.push(RivalTile {
            label: s("WIN RATE"),
            value: s(format!("{:.0}%", wins as f64 * 100.0 / (wins + losses) as f64)),
            sub: s(format!("{wins} of {} fights", wins + losses)),
            dir: 0,
        });
    }

    let mut lines = Vec::new();
    if let Some(best) = fights.iter().filter(|f| f.kind == "kill").filter(|f| f.opp_elo.is_some()).max_by(|a, b| a.opp_elo.unwrap().total_cmp(&b.opp_elo.unwrap())) {
        lines.push(RecapLine {
            label: s("Best win"),
            name: s(best.opponent.as_str()),
            value: s(format!("{} Elo{}", thousands(best.opp_elo.unwrap().round() as i64), best.opp_tier.as_deref().map(|t| format!(" · {t}")).unwrap_or_default())),
            dir: 1,
        });
    }
    let (nemesis, victim) = rivals_of(&fights);
    if let Some(r) = nemesis {
        lines.push(RecapLine { label: s("Nemesis"), name: s(r.name.as_str()), value: s(format!("{}–{}", r.wins, r.losses)), dir: -1 });
    }
    if let Some(r) = victim {
        lines.push(RecapLine { label: s("Favourite victim"), name: s(r.name.as_str()), value: s(format!("{}–{}", r.wins, r.losses)), dir: 1 });
    }
    let upset = |f: &&crate::r5log::Fight| {
        let won = f.kind == "kill";
        f.my_elo.zip(f.opp_elo).map(|(m, o)| win_chance(m, o)).is_some_and(|c| (won && c < 0.35) || (!won && c > 0.65))
    };
    let (uw, ul) = (fights.iter().filter(|f| f.kind == "kill" && upset(f)).count(), fights.iter().filter(|f| f.kind != "kill" && upset(f)).count());
    if uw + ul > 0 {
        lines.push(RecapLine { label: s("Upsets"), name: s(""), value: s(format!("{uw} won · {ul} lost")), dir: 0 });
    }
    if let Some(top) = records(&fights).first().filter(|r| r.wins + r.losses >= 3) {
        lines.push(RecapLine { label: s("Most fought"), name: s(top.name.as_str()), value: s(format!("{}–{}", top.wins, top.losses)), dir: (top.wins - top.losses).signum() as i32 });
    }

    let record = match (wins, losses) {
        (0, 0) => "no fights recorded".to_string(),
        (w, l) => format!("{w} win{} · {l} loss{}", if w == 1 { "" } else { "s" }, if l == 1 { "" } else { "es" }),
    };
    let (tier_from, tier_to) = (before.map(|x| x.tier.clone()).unwrap_or_default(), after.map(|x| x.tier.clone()).unwrap_or_default());
    let note = if now() < sess.end + LAG {
        "x76 books Elo and points a few minutes after the fights, so this recap fills in as they land.".to_string()
    } else if fights.is_empty() {
        format!("No fights from the game log in this session. Turn on {} opponents in Settings to see who you fought.", game.label())
    } else {
        "Fights are from the game log; Elo, points and rank from the ladder checks before and after the session.".to_string()
    };
    Some(RecapPage {
        open: true,
        index: idx as i32,
        key: s(key.as_str()),
        title: s(format!("{} · {}", game.label(), span(sess.end - sess.start))),
        subtitle: s(format!("{} · {} → {}", fav.name, clock_day(sess.start), clock(sess.end))),
        record: s(record),
        lead: (wins - losses).signum() as i32,
        tier_from: s(tier_from.as_str()),
        tier_to: s(tier_to.as_str()),
        tier_key: tier_key(&tier_to),
        tiles: model(tiles),
        lines: model(lines),
        note: s(note),
        has_prev: idx > 0,
        has_next: idx + 1 < core.sessions.len(),
    })
}

// ------------------------------------------------------------------ rivals
/// Your record against one opponent, in some set of fights.
#[derive(Clone, Debug, PartialEq)]
pub struct Rival {
    /// Their name in the newest of these fights.
    pub name: String,
    pub wins: i64,
    pub losses: i64,
}

/// Who an opponent is across fights: their SteamID where the ladder knew it (Quake Live), else
/// their name (names can change, SteamIDs can't).
fn opp_key(f: &crate::r5log::Fight) -> String {
    f.opp_id.clone().unwrap_or_else(|| f.opponent.to_lowercase())
}

/// Every opponent in these fights with your record against them, most fights first.
pub fn records(fights: &[&crate::r5log::Fight]) -> Vec<Rival> {
    let mut by: HashMap<String, (Rival, f64, usize)> = HashMap::new();
    for f in fights.iter().filter(|f| !f.opponent.is_empty()) {
        let e = by.entry(opp_key(f)).or_insert_with(|| (Rival { name: f.opponent.clone(), wins: 0, losses: 0 }, f.t, 0));
        if f.kind == "kill" {
            e.0.wins += 1;
        } else {
            e.0.losses += 1;
        }
        if f.t >= e.1 {
            e.0.name = f.opponent.clone();
            e.1 = f.t;
        }
        e.2 += 1;
    }
    let mut out: Vec<(Rival, f64, usize)> = by.into_values().collect();
    out.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.total_cmp(&a.1)));
    out.into_iter().map(|x| x.0).collect()
}

/// Nemesis (beats you most, at least twice and at least as often as you beat them) and favourite
/// victim (you beat most, at least twice and more often than they beat you).
pub fn rivals_of(fights: &[&crate::r5log::Fight]) -> (Option<Rival>, Option<Rival>) {
    let all = records(fights);
    let nemesis = all
        .iter()
        .filter(|r| r.losses >= 2 && r.losses >= r.wins)
        .max_by(|a, b| (a.losses - a.wins).cmp(&(b.losses - b.wins)).then(a.losses.cmp(&b.losses)))
        .cloned();
    let victim = all
        .iter()
        .filter(|r| r.wins >= 2 && r.wins > r.losses)
        .max_by(|a, b| (a.wins - a.losses).cmp(&(b.wins - b.losses)).then(a.wins.cmp(&b.wins)))
        .cloned();
    (nemesis, victim)
}

/// The head-to-head panel: a tracked player's record against one opponent, from every fight the
/// game logs have.
pub fn rival_page(core: &Core, key: &str, opponent: &str) -> Option<RivalPage> {
    const SHOWN: usize = 25;
    let (game_id, fav_id) = key.split_once(':')?;
    let game = Game::from_id(game_id)?;
    core.state.favorites.iter().find(|f| f.game == game_id && f.id == fav_id)?;
    let mine = fights_for(core, game, fav_id);
    // the opponent by SteamID where known, so a renamed player keeps their history
    let id = mine.iter().rev().find(|f| f.opponent.eq_ignore_ascii_case(opponent)).and_then(|f| f.opp_id.clone());
    let mut fights: Vec<&crate::r5log::Fight> = mine
        .into_iter()
        .filter(|f| f.opponent.eq_ignore_ascii_case(opponent) || (id.is_some() && f.opp_id == id))
        .collect();
    if fights.is_empty() {
        return None;
    }
    fights.sort_by(|a, b| b.t.total_cmp(&a.t)); // newest first
    let newest = fights[0];
    let name = newest.opponent.clone();
    let wins = fights.iter().filter(|f| f.kind == "kill").count() as i64;
    let losses = fights.len() as i64 - wins;
    let chance_of = |f: &crate::r5log::Fight| f.my_elo.zip(f.opp_elo).map(|(m, o)| win_chance(m, o));
    let upset = |f: &crate::r5log::Fight| {
        let won = f.kind == "kill";
        chance_of(f).is_some_and(|c| (won && c < 0.35) || (!won && c > 0.65))
    };

    // them now (latest ladder check), else as they were at the last fight
    let board = core.boards.get(&game);
    let now_p = board.and_then(|bd| {
        id.as_ref()
            .and_then(|i| bd.by_id.get(i))
            .or_else(|| bd.by_id.get(&name))
            .and_then(|v| v.first())
            .map(|&i| &bd.players[i])
            .or_else(|| bd.players.iter().find(|p| p.name.eq_ignore_ascii_case(&name)))
    });
    let their_elo = now_p.and_then(|p| p.elo).or(newest.opp_elo);
    let tier = now_p.map(|p| p.tier.clone()).or_else(|| newest.opp_tier.clone()).unwrap_or_default();
    let rank = now_p.map(|p| p.rank).or(newest.opp_rank);
    let my_elo = core.latest.get(key).and_then(Option::as_ref).and_then(|p| p.elo).or(newest.my_elo);

    let n = fights.len() as i64;
    let mut tiles = vec![RivalTile {
        label: s("RECORD"),
        value: s(format!("{wins}–{losses}")),
        sub: s(format!("{:.0}% won · {} fight{}", wins as f64 * 100.0 / n as f64, n, if n == 1 { "" } else { "s" })),
        dir: (wins - losses).signum() as i32,
    }];
    if let Some(e) = their_elo {
        let gap = my_elo.map(|m| e - m);
        let mut sub = match gap {
            Some(g) if g.abs() >= 0.5 => format!("{} {} you", thousands(g.abs().round() as i64), if g > 0.0 { "above" } else { "below" }),
            Some(_) => "same as you".into(),
            None => String::new(),
        };
        if let Some(m) = my_elo {
            sub = format!("{sub} · {:.0}% on paper", win_chance(m, e) * 100.0);
        }
        tiles.push(RivalTile { label: s("ELO NOW"), value: s(thousands(e.round() as i64)), sub: s(sub), dir: 0 });
    }
    let accs: Vec<(f64, f64)> = fights.iter().filter_map(|f| f.my_acc.zip(f.opp_acc)).collect();
    if !accs.is_empty() {
        let k = accs.len() as f64;
        let (me, them) = (accs.iter().map(|a| a.0).sum::<f64>() / k, accs.iter().map(|a| a.1).sum::<f64>() / k);
        tiles.push(RivalTile {
            label: s("ACCURACY"),
            value: s(format!("{} vs {}", pct(me), pct(them))),
            sub: s(format!("you vs them, {} fight{}", accs.len(), if accs.len() == 1 { "" } else { "s" })),
            dir: if (me - them).abs() < 0.5 { 0 } else if me > them { 1 } else { -1 },
        });
    } else {
        let (uw, ul) = (fights.iter().filter(|f| f.kind == "kill" && upset(f)).count(), fights.iter().filter(|f| f.kind != "kill" && upset(f)).count());
        tiles.push(RivalTile { label: s("UPSETS"), value: s(format!("{uw} · {ul}")), sub: s("won · lost"), dir: 0 });
    }
    let first = fights.last().unwrap();
    tiles.push(RivalTile { label: s("FIRST MET"), value: s(short_when(first.t)), sub: s(format!("last {}", clock_day(newest.t))), dir: 0 });

    let rows: Vec<RivalFight> = fights
        .iter()
        .take(SHOWN)
        .map(|f| {
            let gap = f.opp_elo.zip(f.my_elo).map(|(o, m)| o - m);
            RivalFight {
                when: s(clock_day(f.t)),
                won: f.kind == "kill",
                elo: s(f.opp_elo.map(|e| thousands(e.round() as i64)).unwrap_or_default()),
                gap: s(gap.filter(|g| g.abs() >= 0.5).map(|g| thousands(g.abs().round() as i64)).unwrap_or_default()),
                gap_dir: gap.map(|g| if g >= 0.5 { 1 } else if g <= -0.5 { -1 } else { 0 }).unwrap_or(0),
                acc: s(match (f.my_acc, f.opp_acc) {
                    (Some(m), Some(o)) => format!("{} · {}", pct(m), pct(o)),
                    _ => String::new(),
                }),
                upset: upset(f),
            }
        })
        .collect();
    let lead = (wins - losses).signum() as i32;
    let headline = match lead {
        1 => format!("You lead {wins}–{losses}"),
        -1 => format!("{name} leads {losses}–{wins}"),
        _ => format!("Even at {wins}–{losses}"),
    };
    let tracked = core.state.favorites.iter().any(|f| f.game == game_id && (Some(&f.id) == id.as_ref() || f.id.eq_ignore_ascii_case(&name)));
    let track_id = if game == Game::Ql { id.clone().unwrap_or_default() } else { name.clone() };
    Some(RivalPage {
        open: true,
        key: s(key),
        game: s(game_id),
        name: s(name.as_str()),
        tier: s(tier.as_str()),
        tier_key: if tier.is_empty() { -1 } else { tier_key(&tier) },
        rank: s(rank.map(|r| format!("#{}", thousands(r))).unwrap_or_default()),
        headline: s(headline),
        lead,
        tiles: model(tiles),
        fights: model(rows),
        more: s(if fights.len() > SHOWN { format!("and {} older fights", fights.len() - SHOWN) } else { String::new() }),
        can_track: !tracked && !track_id.is_empty(),
        track_id: s(track_id),
        has_acc: !accs.is_empty(),
        hint: s(format!(
            "Every fight with {name} that x76tracker has read from {}. Elo and the gap are from the last ladder check before each fight.",
            if game == Game::Ql { "Quake Live's console log" } else { "R5Reloaded's game log" }
        )),
    })
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

/// How far back Discord's "recent" numbers look.
pub const RECENT_SECS: f64 = 48.0 * 3600.0;

/// One tracked player's current numbers and their last 48 hours, for Discord (presence and widget).
#[derive(Clone, Debug, Default)]
pub struct PlayerStats {
    pub rank: Option<i64>,
    pub points: Option<i64>,
    pub elo: Option<f64>,
    pub tier: String,
    pub kills: i64,
    pub deaths: i64,
    /// Change over the last 48 hours (or since tracking started, if that's more recent).
    pub recent_elo: Option<f64>,
    pub recent_points: Option<i64>,
    pub recent_wins: i64,
    pub recent_losses: i64,
    /// The tier 48 hours ago (or when tracking started).
    pub recent_tier_from: String,
    pub peak_elo: Option<f64>,
    /// Highest-rated opponent beaten in the last 48 hours (from the game log: R5Reloaded's, or Quake Live's console log).
    pub best_win: Option<(String, f64)>,
}

pub fn player_stats(core: &Core, game: Game, id: &str) -> Option<PlayerStats> {
    let key = key_of(game.id(), id);
    let h: &[Snap] = core.state.history.get(&key).map(Vec::as_slice).unwrap_or(&[]);
    let live: Option<&Player> = core.latest.get(&key).and_then(Option::as_ref);
    let last = h.last();
    if live.is_none() && last.is_none() {
        return None;
    }
    let points = live.and_then(|p| p.points).or_else(|| last.and_then(|s| s.points));
    let elo = live.and_then(|p| p.elo).or_else(|| last.and_then(|s| s.elo));
    let start = now() - RECENT_SECS;
    let base = baseline(h, start);
    let (wins, losses) = events(h, start).iter().fold((0, 0), |(w, l), e| (w + e.w, l + e.l));
    let peak_elo = h.iter().filter_map(|s| s.elo).chain(elo).reduce(f64::max);
    let best_win = fights_for(core, game, id)
        .into_iter()
        .filter(|f| f.kind == "kill" && f.t >= start)
        .filter_map(|f| f.opp_elo.map(|e| (f.opponent.clone(), e)))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    Some(PlayerStats {
        rank: live.map(|p| p.rank).or_else(|| last.and_then(|s| s.rank)),
        points,
        elo,
        tier: live.map(|p| p.tier.clone()).or_else(|| last.map(|s| s.tier.clone())).unwrap_or_default(),
        kills: live.map(|p| p.kills).or_else(|| last.map(|s| s.kills)).unwrap_or(0),
        deaths: live.map(|p| p.deaths).or_else(|| last.map(|s| s.deaths)).unwrap_or(0),
        recent_elo: base.and_then(|b| elo.zip(b.elo)).map(|(c, b)| c - b),
        recent_points: base.and_then(|b| points.zip(b.points)).map(|(c, b)| c - b),
        recent_wins: wins,
        recent_losses: losses,
        recent_tier_from: base.map(|b| b.tier.clone()).unwrap_or_default(),
        peak_elo,
        best_win,
    })
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

// ------------------------------------------------------------------ result detail panel
/// Identifies a history row (its timestamp, as saved).
pub fn row_id(t: f64) -> String {
    format!("{t:.1}")
}

fn two_decimals(v: f64) -> String {
    let hundredths = (v.abs() * 100.0).round() as i64;
    let s = format!("{}.{:02}", thousands(hundredths / 100), hundredths % 100);
    if v < 0.0 && hundredths != 0 { format!("-{s}") } else { s }
}

fn clock_secs(t: f64) -> String {
    local(t).format("%-I:%M:%S %p").to_string()
}

/// Your chance to win on paper, from the standard Elo formula.
fn win_chance(me: f64, opp: f64) -> f64 {
    1.0 / (1.0 + 10f64.powf((opp - me) / 400.0))
}

fn change(label: &str, before: Option<f64>, after: Option<f64>, fmt_v: &dyn Fn(f64) -> String, fmt_d: &dyn Fn(f64) -> String, eps: f64, up_is_good: bool) -> StatChange {
    let d = before.zip(after).map(|(b, a)| a - b);
    let dir = d.map(|d| if d > eps { 1 } else if d < -eps { -1 } else { 0 }).unwrap_or(0);
    StatChange {
        label: s(label),
        before: s(before.map(fmt_v).unwrap_or_else(|| "–".into())),
        after: s(after.map(fmt_v).unwrap_or_else(|| "–".into())),
        change: s(match d {
            Some(d) if dir != 0 => fmt_d(d.abs()),
            Some(_) => "no change".into(),
            None => "–".into(),
        }),
        dir: if up_is_good { dir } else { -dir },
    }
}

/// Everything about one result row: before/after numbers and each fight in it.
pub fn event_detail(core: &Core, key: &str, id: &str) -> Option<EventDetail> {
    let (game_id, fav_id) = key.split_once(':')?;
    let game = Game::from_id(game_id)?;
    let fav = core.state.favorites.iter().find(|f| f.game == game_id && f.id == fav_id)?;
    let h = core.state.history.get(key)?;
    let i = h.iter().position(|snap| row_id(snap.t) == id)?;
    if i == 0 {
        return None;
    }
    let (a, b) = (&h[i - 1], &h[i]);

    let kd = |k: i64, d: i64| if d > 0 { k as f64 / d as f64 } else { k as f64 };
    let stats = vec![
        change("POINTS", a.points.map(|v| v as f64), b.points.map(|v| v as f64), &|v| format!("{}P", thousands(v as i64)), &|d| format!("{}P", thousands(d.round() as i64)), 0.5, true),
        change("ELO", a.elo, b.elo, &two_decimals, &two_decimals, 0.005, true),
        // a lower rank number is better, so "up" is a smaller number
        change("RANK", a.rank.map(|v| v as f64), b.rank.map(|v| v as f64), &|v| format!("#{}", thousands(v as i64)), &|d| format!("{} places", thousands(d.round() as i64)), 0.5, false),
        change("K/D", Some(kd(a.kills, a.deaths)), Some(kd(b.kills, b.deaths)), &|v| format!("{v:.3}"), &|d| format!("{d:.3}"), 0.0005, true),
    ];

    let fights: Vec<&crate::r5log::Fight> = fights_for(core, game, fav_id).into_iter().filter(|f| f.t > a.t && f.t <= b.t).collect();
    let (dw, dl) = ((b.kills - a.kills).max(0), (b.deaths - a.deaths).max(0));
    let (w, l) = if dw == 0 && dl == 0 && !fights.is_empty() {
        (fights.iter().filter(|f| f.kind == "kill").count() as i64, fights.iter().filter(|f| f.kind == "death").count() as i64)
    } else {
        (dw, dl)
    };
    let record = match (w, l) {
        (0, 0) => "no games in this change".to_string(),
        (w, 0) => format!("{w} win{}", if w == 1 { "" } else { "s" }),
        (0, l) => format!("{l} loss{}", if l == 1 { "" } else { "es" }),
        (w, l) => format!("{w} win{} · {l} loss{}", if w == 1 { "" } else { "s" }, if l == 1 { "" } else { "es" }),
    };

    let board = core.boards.get(&game);
    let tracked: Vec<&str> = core.state.favorites.iter().filter(|f| f.game == game_id).map(|f| f.id.as_str()).collect();
    let fight_rows: Vec<FightDetail> = fights
        .iter()
        .map(|f| {
            let won = f.kind == "kill";
            let gap = f.opp_elo.zip(f.my_elo).map(|(o, m)| o - m);
            let chance = f.my_elo.zip(f.opp_elo).map(|(m, o)| win_chance(m, o));
            let upset = chance.is_some_and(|c| (won && c < 0.35) || (!won && c > 0.65));
            // the opponent as they are now, from the latest ladder check (Quake Live: by SteamID)
            let opp_id = f.opp_id.clone().unwrap_or_else(|| f.opponent.clone());
            let now_p = board.and_then(|bd| {
                bd.by_id
                    .get(&opp_id)
                    .and_then(|v| v.first())
                    .map(|&i| &bd.players[i])
                    .or_else(|| bd.players.iter().find(|p| p.name.eq_ignore_ascii_case(&f.opponent)))
            });
            let now = now_p
                .map(|p| {
                    let mut parts = vec![format!("k/d {:.2}", kd(p.kills, p.deaths))];
                    if let Some(acc) = p.acc {
                        parts.push(format!("acc {acc:.0}%"));
                    }
                    if let Some(p24) = p.points_24h {
                        parts.push(format!("24h {}{}P", if p24 > 0 { "+" } else if p24 < 0 { "-" } else { "±" }, thousands(p24.abs())));
                    }
                    parts.push(format!("{} games", thousands(p.kills + p.deaths)));
                    format!("now: {}", parts.join(" · "))
                })
                .unwrap_or_default();
            FightDetail {
                when: s(clock_secs(f.t)),
                won,
                opponent: s(if f.opponent.is_empty() { "unknown" } else { f.opponent.as_str() }),
                tier: s(f.opp_tier.clone().unwrap_or_default()),
                tier_key: f.opp_tier.as_deref().map(tier_key).unwrap_or(-1),
                rank: s(f.opp_rank.map(|r| format!("#{}", thousands(r))).unwrap_or_default()),
                elo: s(f.opp_elo.map(two_decimals).unwrap_or_default()),
                points: s(f.opp_points.map(|p| format!("{}P", thousands(p))).unwrap_or_default()),
                gap: s(gap.map(|g| two_decimals(g.abs())).unwrap_or_default()),
                gap_dir: gap.map(|g| if g > 0.005 { 1 } else if g < -0.005 { -1 } else { 0 }).unwrap_or(0),
                chance: s(chance.map(|c| format!("{:.0}% chance on paper", c * 100.0)).unwrap_or_default()),
                upset,
                now: s(now),
                rated: f.opp_elo.is_some(),
                // Quake Live players are tracked by SteamID, so only once the ladder knows who it was
                can_track: !f.opponent.is_empty()
                    && (game == Game::R5 || f.opp_id.is_some())
                    && !tracked.iter().any(|t| t.eq_ignore_ascii_case(&opp_id)),
                track_id: s(opp_id.as_str()),
                acc: s(match (f.my_acc, f.opp_acc) {
                    (Some(m), Some(o)) => format!("accuracy: you {} · them {}", pct(m), pct(o)),
                    (Some(m), None) => format!("accuracy: you {}", pct(m)),
                    (None, Some(o)) => format!("accuracy: them {}", pct(o)),
                    _ => String::new(),
                }),
            }
        })
        .collect();

    let hint = if fight_rows.is_empty() && game == Game::Ql {
        "No fights from the console log in this change. To see who you fight, turn on Quake Live opponents in Settings.".to_string()
    } else if fight_rows.is_empty() {
        "No fights from the game log in this change. To see who you fight, turn on R5Reloaded opponents in Settings.".to_string()
    } else if fight_rows.iter().any(|f| !f.rated) {
        "Unrated opponents weren't on the ladder, or were read too long after the fight to know their rating then.".to_string()
    } else {
        "Opponent numbers are from the last ladder check before each fight. Chance on paper uses the standard Elo formula.".to_string()
    };

    Some(EventDetail {
        open: true,
        key: s(key),
        game: s(game_id),
        title: s(format!("{} · {}", clock_secs(b.t), local(b.t).format("%a %b %-d"))),
        subtitle: s(format!("{} · {} · ladder checks {} → {}", fav.name, game.label(), clock_secs(a.t), clock_secs(b.t))),
        stats: model(stats),
        record: s(record),
        tier_from: s(a.tier.as_str()),
        tier_to: s(b.tier.as_str()),
        tier_key: tier_key(&b.tier),
        fights: model(fight_rows),
        hint: s(hint),
    })
}

// ------------------------------------------------------------------ profile card
fn rank_tile(core: &Core, game: Game, id: Option<&str>) -> GameRank {
    let mut r = GameRank { label: s(game.label().to_uppercase()), ..Default::default() };
    let Some(id) = id else { return r };
    r.name = s(id);
    let found: Option<Player> = core
        .boards
        .get(&game)
        .and_then(|b| b.by_id.get(id).and_then(|v| v.first()).map(|&i| b.players[i].clone()))
        .or_else(|| core.latest.get(&key_of(game.id(), id)).cloned().flatten());
    if let Some(p) = found {
        r.has = true;
        r.rank = s(format!("#{}", thousands(p.rank)));
        r.tier = s(p.tier.as_str());
        r.tier_key = tier_key(&p.tier);
        r.points = s(p.points.map(thousands).unwrap_or_else(|| "–".into()));
        r.elo = s(p.elo.map(|e| thousands(e.round() as i64)).unwrap_or_else(|| "–".into()));
    }
    r
}

/// The x76 profile card: who you are on x76 plus your ladder standing in both games.
pub fn profile_card(core: &Core, has_avatar: bool, has_banner: bool) -> ProfileCard {
    let mut c = ProfileCard {
        busy: core.profile_busy,
        error: s(core.profile_error.clone().unwrap_or_default()),
        ..Default::default()
    };
    let Some(p) = &core.profile else { return c };
    c.linked = true;
    c.name = s(p.display_name.as_str());
    c.handle = s(p.handle.as_str());
    c.supporter = p.supporter;
    c.verified = p.verified;
    c.has_avatar = has_avatar;
    c.has_banner = has_banner;
    c.initial = s(p.display_name.chars().next().map(|ch| ch.to_uppercase().to_string()).unwrap_or_default());
    let badges: Vec<SharedString> = p
        .badges
        .iter()
        .filter(|b| !(p.supporter && b.as_str() == "supporter"))
        .map(|b| s(b.to_lowercase()))
        .collect();
    c.badges = model(badges);
    // Quake Live through the SteamID on the profile; R5 through your in-game name from the game log
    c.ql = rank_tile(core, Game::Ql, p.steam_id.as_deref());
    let r5_name = core
        .fights
        .last()
        .map(|f| f.me.clone())
        .or_else(|| core.state.favorites.iter().find(|f| f.game == "r5").map(|f| f.id.clone()));
    c.r5 = rank_tile(core, Game::R5, r5_name.as_deref());
    c
}

// ------------------------------------------------------------------ share card
/// The share card: your x76 profile (if linked) and your players' ranks (Settings → your players).
pub fn share_card(core: &Core, has_avatar: bool, has_banner: bool) -> ShareCard {
    let cfg = crate::discord::Config::load(core);
    let mut games = Vec::new();
    let mut names = Vec::new();
    for g in [Game::R5, Game::Ql] {
        let Some(id) = cfg.player(core, g) else { continue };
        let Some(st) = player_stats(core, g, &id) else { continue };
        names.push(core.state.favorites.iter().find(|f| f.game == g.id() && f.id == id).map(|f| f.name.clone()).unwrap_or_else(|| id.clone()));
        let (change, change_dir) = match st.recent_points {
            Some(d) if d != 0 => (format!("{}P", thousands(d.abs())), d.signum() as i32),
            _ => (String::new(), 0),
        };
        let played = st.recent_wins + st.recent_losses;
        // points over the same 48 hours, for the sparkline
        let spark = core.state.history.get(&key_of(g.id(), &id)).and_then(|h| chart(h, now() - RECENT_SECS, false));
        games.push(ShareGame {
            has_chart: spark.is_some(),
            chart: spark.unwrap_or_default(),
            label: s(g.label().to_uppercase()),
            tier: s(if st.tier.is_empty() { "Unranked".to_string() } else { st.tier.clone() }),
            tier_key: tier_key(&st.tier),
            rank: s(st.rank.map(|r| format!("#{}", thousands(r))).unwrap_or_default()),
            points: s(st.points.map(thousands).unwrap_or_else(|| "–".into())),
            elo: s(st.elo.map(|e| thousands(e.round() as i64)).unwrap_or_else(|| "–".into())),
            change: s(change),
            change_dir,
            record: s(if played > 0 { format!("{}-{} · 48h", st.recent_wins, st.recent_losses) } else { "no games · 48h".to_string() }),
        });
    }
    let profile = core.profile.as_ref();
    let name = profile.map(|p| p.display_name.clone()).or_else(|| names.first().cloned()).unwrap_or_else(|| "x76 player".into());
    ShareCard {
        ok: profile.is_some() || !games.is_empty(),
        initial: s(name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()),
        name: s(name),
        handle: s(profile.map(|p| p.handle.clone()).unwrap_or_default()),
        supporter: profile.is_some_and(|p| p.supporter),
        has_avatar: profile.is_some() && has_avatar,
        has_banner: profile.is_some() && has_banner,
        date: s(Local::now().format("%b %-d, %Y").to_string()),
        games: model(games),
    }
}

// ------------------------------------------------------------------ servers page
/// Step chart of a whole-number series (player counts) over time.
fn count_chart(pts: &[(f64, f64)], word: &str) -> Option<ChartData> {
    if pts.len() < 2 {
        return None;
    }
    let (t0, t1) = (pts[0].0, now().max(pts.last().unwrap().0 + 1.0));
    let mut lo = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let mut hi = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    if hi - lo < 4.0 {
        let c = (hi + lo) / 2.0;
        lo = c - 2.0;
        hi = c + 2.0;
    }
    lo = (lo - (hi - lo) * 0.14).max(-0.5);
    hi += (hi - lo) * 0.14;
    let fx = |t: f64| ((t - t0) / (t1 - t0)) as f32;
    let fy = |v: f64| (1.0 - (v - lo) / (hi - lo)) as f32;
    let step = nice_step((hi - lo) / 3.0).max(1.0);
    let mut ticks = Vec::new();
    let mut v = (lo / step).ceil() * step;
    while v <= hi {
        ticks.push(Tick { y: fy(v), label: s(thousands(v.round() as i64)) });
        v += step;
    }
    let points: Vec<ChartPoint> = pts
        .iter()
        .enumerate()
        .map(|(i, &(t, v))| {
            let (change, d) = if i == 0 { (String::new(), 0) } else { delta_text(false, Some(v - pts[i - 1].1), "", "no change") };
            ChartPoint {
                x: fx(t),
                y: fy(v),
                value: s(thousands(v.round() as i64)),
                when: s(format!("at {}", clock_day(t)).to_uppercase()),
                change: s(change),
                dir: d,
                sub: s(word),
            }
        })
        .collect();
    let lookup: Vec<i32> = (0..200)
        .map(|b| {
            let f = (b as f32 + 0.5) / 200.0;
            points.iter().rposition(|p| p.x <= f).unwrap_or(0) as i32
        })
        .collect();
    Some(ChartData {
        ticks: model(ticks),
        points: model(points),
        lookup: model(lookup),
        start: s(if is_today(t0) { clock(t0) } else { local(t0).format("%b %-d, %-I:%M %p").to_string() }),
        span: s(span(t1 - t0)),
    })
}

fn server_game(core: &Core, game: &str, label: &str, pick: fn(&crate::servers::Sample) -> i64) -> ServerGame {
    let list: Vec<&crate::servers::Server> = core.servers.iter().filter(|x| x.game == game).collect();
    let online: Vec<&&crate::servers::Server> = list.iter().filter(|x| x.online).collect();
    let total: i64 = online.iter().map(|x| x.players).sum();
    let slots: i64 = online.iter().map(|x| x.max).sum();
    // today's peak, from the samples the app took
    let midnight = local_midnight();
    let peak = core.server_history.iter().filter(|x| x.t >= midnight).max_by_key(|x| pick(x));
    // the chart covers the last day, from the most recent stretch the app was running without a gap
    let day: Vec<&crate::servers::Sample> = core.server_history.iter().filter(|x| x.t >= now() - 86_400.0).collect();
    let start = day.windows(2).rposition(|w| w[1].t - w[0].t > 600.0).map(|i| i + 1).unwrap_or(0);
    let pts: Vec<(f64, f64)> = day[start..].iter().map(|x| (x.t, pick(x) as f64)).collect();
    let chart = count_chart(&pts, "players");
    let mut rows: Vec<ServerRow> = list
        .iter()
        .map(|x| ServerRow {
            name: s(x.name.as_str()),
            region: s(x.region.as_str()),
            map: s(x.map.as_str()),
            players: x.players as i32,
            max: x.max as i32,
            fill: if x.max > 0 { (x.players as f32 / x.max as f32).clamp(0.0, 1.0) } else { 0.0 },
            online: x.online,
        })
        .collect();
    // busiest first, offline last
    rows.sort_by(|a, b| b.online.cmp(&a.online).then(b.players.cmp(&a.players)).then(a.region.cmp(&b.region)).then(a.name.cmp(&b.name)));
    ServerGame {
        label: s(label),
        total: total as i32,
        summary: s(if list.is_empty() {
            "no servers listed".to_string()
        } else {
            format!("{} of {} servers online · {} / {} slots", online.len(), list.len(), thousands(total), thousands(slots))
        }),
        peak: s(peak.map(|x| thousands(pick(x))).unwrap_or_else(|| "–".into())),
        peak_at: s(peak.map(|x| format!("at {}", clock(x.t))).unwrap_or_default()),
        has_chart: chart.is_some(),
        chart: chart.unwrap_or_default(),
        rows: model(rows),
    }
}

pub fn servers_page(core: &Core) -> ServersPage {
    ServersPage {
        loaded: core.servers_at.is_some(),
        ql: server_game(core, crate::servers::QUAKE, "Quake Live", |x| x.ql),
        r5: server_game(core, crate::servers::R5, "R5Reloaded", |x| x.r5),
    }
}

pub fn servers_status(core: &Core) -> String {
    match (&core.servers_error, core.servers_at) {
        (Some(e), _) => format!("last check failed: {e}"),
        (None, Some(t)) => format!("checked {} · every minute", ago(Some(t))),
        (None, None) => "checking x76's servers…".into(),
    }
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

// ------------------------------------------------------------------ settings: Discord
fn slot_kind(kind: &str) -> i32 {
    match kind {
        "tile" => 0,
        "image" => 2,
        "card" => 3,
        "progress" => 4,
        _ => 1,
    }
}

/// Tracked players of one game for the "your players" pickers: (names, chosen index).
fn player_choices(core: &Core, cfg: &crate::discord::Config, game: Game) -> (Vec<String>, i32) {
    let favs: Vec<&Favorite> = core.state.favorites.iter().filter(|f| f.game == game.id()).collect();
    if favs.is_empty() {
        return (vec![format!("No {} player tracked yet", game.label())], 0);
    }
    let chosen = cfg.player(core, game);
    let i = favs.iter().position(|f| Some(&f.id) == chosen.as_ref()).unwrap_or(0);
    (favs.iter().map(|f| f.name.clone()).collect(), i as i32)
}

pub fn discord_page(core: &Core) -> DiscordPage {
    use crate::discord::{options, Config, APP_ID};
    let cfg = Config::load(core);
    let names = |kind: &str| model(options(kind).into_iter().map(|(_, m)| s(m)).collect());
    let slots: Vec<WidgetSlot> = cfg
        .layout
        .iter()
        .map(|slot| {
            let source = cfg.source_of(slot);
            WidgetSlot {
                key: s(slot.key.as_str()),
                kind: slot_kind(&slot.kind),
                surface: s(slot.surface.as_str()),
                name: s(slot.name.as_str()),
                source: options(&slot.kind).iter().position(|(id, _)| *id == source).unwrap_or(0) as i32,
                custom: s(cfg.custom_of(slot)),
                is_custom: source == "custom",
            }
        })
        .collect();
    let statics: Vec<StaticText> = cfg
        .statics
        .iter()
        .map(|f| StaticText { surface: s(f.surface.as_str()), name: s(f.name.as_str()), text: s(f.text.as_str()) })
        .collect();
    let r5_dir = core.setting_str("r5Path");
    let (r5_names, r5_i) = player_choices(core, &cfg, Game::R5);
    let (ql_names, ql_i) = player_choices(core, &cfg, Game::Ql);
    let rpc_app = if cfg.rpc_app() == APP_ID {
        "Shows as “Playing x76tracker”.".to_string()
    } else {
        let name = if cfg.widget_name.is_empty() { "your widget's app".to_string() } else { format!("your widget's app (“{}”)", cfg.widget_name) };
        format!("Shows as {name}, so the widget's “while playing” panel appears with it. Change this under Discord profile widget, below.")
    };
    DiscordPage {
        rpc: cfg.rpc,
        rpc_app: s(rpc_app),
        r5_patch: core.r5patch == Some(crate::r5patch::State::On),
        r5_found: r5_dir.is_some(),
        ql_log: core.qllog == Some(true),
        recap_auto: core.state.settings.get("recapAuto").and_then(serde_json::Value::as_bool).unwrap_or(true),
        players_r5: model(r5_names.into_iter().map(s).collect()),
        player_r5: r5_i,
        players_ql: model(ql_names.into_iter().map(s).collect()),
        player_ql: ql_i,
        widget: cfg.widget,
        app_id: s(cfg.app_id.as_str()),
        user_id: s(cfg.user_id.as_str()),
        has_token: core.discord.token.is_some(),
        widget_rpc: cfg.widget_rpc,
        slots: model(slots),
        statics: model(statics),
        tile_options: names("tile"),
        text_options: names("text"),
        image_options: names("image"),
        card_options: names("card"),
        progress_options: names("progress"),
    }
}

/// What each widget field would show right now (same order as the page's slots).
pub fn discord_previews(core: &Core) -> ModelRc<SharedString> {
    let cfg = crate::discord::Config::load(core);
    model(cfg.layout.iter().map(|slot| s(crate::discord::preview(core, &cfg, slot))).collect())
}

// ------------------------------------------------------------------ settings: updates
/// "New" green, "Improved" blue, "Fixed" red, anything else plain.
fn section_kind(title: &str) -> i32 {
    match title.to_lowercase().as_str() {
        t if t.starts_with("new") || t.starts_with("added") => 0,
        t if t.starts_with("improv") || t.starts_with("changed") => 1,
        t if t.starts_with("fix") => 2,
        _ => 3,
    }
}

/// The What's new menu: a newer release on GitHub first (with its notes), then CHANGELOG.md.
/// `media` turns a picture reference into what the UI shows (loading, ready, failed).
pub fn changelog(core: &Core, media: &dyn Fn(&crate::update::Media) -> ChangeMedia) -> Vec<ChangeEntry> {
    use crate::update::{parse_changelog, CHANGELOG};
    let mut entries: Vec<(crate::update::Entry, bool)> = Vec::new();
    if let Some(r) = core.updater.newer() {
        entries.extend(parse_changelog(&r.notes, &r.version).into_iter().take(1).map(|e| (e, true)));
    }
    entries.extend(parse_changelog(CHANGELOG, "").into_iter().map(|e| (e, false)));
    entries
        .into_iter()
        .map(|(e, available)| ChangeEntry {
            current: e.version == crate::tracker::VERSION,
            available,
            hero: e.hero.as_ref().map(media).unwrap_or_default(),
            note: s(e.note.as_str()),
            sections: model(
                e.sections
                    .iter()
                    .map(|sec| ChangeSection {
                        title: s(sec.title.to_uppercase()),
                        kind: section_kind(&sec.title),
                        items: model(
                            sec.items
                                .iter()
                                .map(|it| ChangeItem {
                                    title: s(it.lead.as_str()),
                                    text: s(it.text.as_str()),
                                    media: it.media.as_ref().map(media).unwrap_or_default(),
                                })
                                .collect(),
                        ),
                    })
                    .collect(),
            ),
            version: s(e.version),
        })
        .collect()
}

pub fn updates_page(core: &Core) -> UpdatesPage {
    use crate::update::{auto_check, auto_install, dev_build, RELEASES_PAGE};
    let u = &core.updater;
    let newer = u.newer();
    let dev = dev_build();
    UpdatesPage {
        version: s(crate::tracker::VERSION),
        auto_check: auto_check(core),
        auto_install: auto_install(core),
        dev_build: dev,
        available: s(newer.map(|r| r.version.clone()).unwrap_or_default()),
        can_install: newer.is_some() && !dev,
        busy: u.checking || u.progress.is_some(),
        page: s(newer.map(|r| r.page.clone()).unwrap_or_else(|| RELEASES_PAGE.to_string())),
    }
}

/// The Updates tab's status line: (text, state as for Discord's lines).
pub fn update_status(core: &Core) -> (String, i32) {
    use crate::update::{auto_check, auto_install, dev_build};
    let u = &core.updater;
    let newer = u.newer();
    if let Some(p) = u.progress {
        let v = newer.map(|r| r.version.as_str()).unwrap_or("the update");
        return (format!("Downloading {v}… {}%", (p * 100.0).round()), 1);
    }
    if u.checking {
        return ("Checking GitHub for a new version…".into(), 1);
    }
    if let Some(e) = &u.error {
        return (e.clone(), 3);
    }
    if let Some(r) = newer {
        let dev = dev_build();
        return match &u.staged {
            Some((v, _)) if *v == r.version && auto_install(core) && !dev => (format!("{v} is downloaded and checked. It's installed when you close x76tracker."), 2),
            Some((v, _)) if *v == r.version => (format!("{v} is downloaded and checked. Press Install to restart into it."), 2),
            _ if dev => (
                format!("{} is available. This copy runs from cargo's build folder, so it never replaces itself; rebuild instead.", r.version),
                1,
            ),
            _ => (format!("{} is available.", r.version), 1),
        };
    }
    match u.checked_at {
        Some(t) => (format!("You're up to date. Checked {}.", ago(Some(t))), 2),
        None if !auto_check(core) => ("Automatic checks are off.".into(), 0),
        None => ("Not checked yet.".into(), 0),
    }
}

/// The R5Reloaded patch's status line: (text, state as for Discord's lines).
pub fn patch_status(core: &Core) -> (String, i32) {
    use crate::r5patch::{script_path, State};
    let Some(dir) = core.setting_str("r5Path") else {
        return ("R5Reloaded wasn't found in the usual places. Enter its folder below (the one with r5apex.exe).".into(), 3);
    };
    let error = core.r5patch_note.as_ref().filter(|(err, _)| *err).map(|(_, t)| t.clone());
    match (core.r5patch, error) {
        (_, Some(e)) => (e, 3),
        (Some(State::On), _) => {
            let extra = core.r5patch_note.as_ref().map(|(_, t)| format!(" {t}")).unwrap_or_default();
            (format!("On: {} is patched. It takes effect the next time the game starts.{extra}", script_path(std::path::Path::new(&dir)).display()), 2)
        }
        (Some(State::Off), _) => ("Off. The game's script is untouched.".into(), 0),
        (Some(State::NoScript), _) => ("This R5Reloaded folder has no cl_obituary.gnut, so there's nothing to patch.".into(), 3),
        (None, _) => ("Checking the game folder…".into(), 1),
    }
}

/// Quake Live's console log switch: (text, state as for Discord's lines).
pub fn ql_log_status(core: &Core) -> (String, i32) {
    let Some(dir) = core.setting_str("qlPath") else {
        return (r"Quake Live wasn't found. Its account folder (Quake Live\<SteamID>\baseq3) turns up once the game has run.".into(), 3);
    };
    let dir = std::path::Path::new(&dir);
    let log = dir.join("qconsole.log");
    let written = log.metadata().and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).map(|e| e.as_secs_f64());
    let whose = crate::qllog::account_of(dir).map(|id| format!(" of SteamID {id}")).unwrap_or_default();
    let note = core.qllog_note.as_ref();
    if let Some((true, e)) = note {
        return (e.clone(), 3);
    }
    let extra = note.map(|(_, t)| format!(" {t}")).unwrap_or_default();
    match core.qllog {
        Some(true) => match written {
            Some(age) if age < 600.0 => (format!("On, and reading the console log{whose}."), 2),
            _ => (format!("On: autoexec.cfg turns the console log on when Quake Live starts.{extra}"), 1),
        },
        Some(false) => match written {
            // switched on in the console by hand
            Some(age) if age < 600.0 => (format!("Off in autoexec.cfg, but logfile is on in the game right now, so the console log{whose} is read anyway."), 2),
            _ => ("Off. autoexec.cfg is untouched.".into(), 0),
        },
        None => ("Checking the game folder…".into(), 1),
    }
}

pub struct DiscordStatus {
    pub rpc: (String, i32),
    pub widget: (String, i32),
    /// (text, is_error, busy)
    pub layout: (String, bool, bool),
}

/// Status lines for Rich Presence and the widget. State: 0 off, 1 waiting, 2 working, 3 problem.
pub fn discord_status(core: &Core) -> DiscordStatus {
    use crate::discord::{valid_id, Config};
    let cfg = Config::load(core);
    let d = &core.discord;
    let rpc = if !cfg.rpc {
        ("Off.".to_string(), 0)
    } else if let Some(e) = &d.rpc_error {
        (e.clone(), 3)
    } else if d.rpc_showing {
        let game = d.playing.map(Game::label).unwrap_or("your game");
        let who = d.rpc_user.as_ref().map(|u| format!(" for {u}")).unwrap_or_default();
        (format!("Showing {game} on Discord{who}."), 2)
    } else if let Some(g) = d.playing {
        (format!("{} is running; connecting to Discord…", g.label()), 1)
    } else {
        ("On. It appears once R5Reloaded or Quake Live is running.".to_string(), 1)
    };
    let widget = if !cfg.widget {
        ("Off.".to_string(), 0)
    } else if d.token.is_none() {
        ("Paste your app's bot token above to start.".to_string(), 3)
    } else if !valid_id(&cfg.app_id) {
        ("Enter your app's Application ID above.".to_string(), 3)
    } else if !valid_id(&cfg.user_id) {
        ("Enter your Discord user ID above, or open Discord on this PC and x76tracker reads it from there.".to_string(), 3)
    } else if cfg.layout.is_empty() {
        ("Press Read widget layout.".to_string(), 3)
    } else if let Some(e) = &d.error {
        (e.clone(), 3)
    } else if let Some(t) = d.ok_at {
        (format!("Widget updated {}. The next update goes out when your numbers change.", ago(Some(t))), 2)
    } else {
        ("Sending the first update…".to_string(), 1)
    };
    let layout = match &d.layout_note {
        _ if d.layout_busy => ("Reading the widget layout from Discord…".to_string(), false, true),
        Some((err, text)) => (text.clone(), *err, false),
        None if !cfg.layout.is_empty() => {
            let name = if cfg.widget_name.is_empty() { "The widget".to_string() } else { format!("“{}”", cfg.widget_name) };
            (format!("{name} has {} field(s) x76tracker can fill.", cfg.layout.len()), false, false)
        }
        None => (String::new(), false, false),
    };
    DiscordStatus { rpc, widget, layout }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::r5log::Fight;

    fn fight(t: f64, kind: &str, opp: &str, id: Option<&str>) -> Fight {
        Fight { t, kind: kind.into(), me: "me".into(), opponent: opp.into(), opp_id: id.map(String::from), ..Default::default() }
    }

    #[test]
    fn records_and_rivals() {
        let fs = vec![
            fight(1.0, "death", "am9u", Some("1")),
            fight(2.0, "death", "am9u", Some("1")),
            fight(3.0, "kill", "am9u", Some("1")),
            // a rename: same SteamID, so the same opponent, shown by their newest name
            fight(4.0, "death", "am9u2", Some("1")),
            fight(5.0, "kill", "chamame", None),
            fight(6.0, "kill", "Chamame", None),
            fight(7.0, "kill", "ed", None),
            fight(8.0, "death", "", None),
        ];
        let refs: Vec<&Fight> = fs.iter().collect();
        let all = records(&refs);
        assert_eq!(all[0], Rival { name: "am9u2".into(), wins: 1, losses: 3 });
        assert_eq!(all[1], Rival { name: "Chamame".into(), wins: 2, losses: 0 });
        assert_eq!(all.len(), 3); // the unknown killer isn't an opponent
        let (nemesis, victim) = rivals_of(&refs);
        assert_eq!(nemesis.map(|r| r.name), Some("am9u2".into()));
        assert_eq!(victim.map(|r| r.name), Some("Chamame".into()));
        // one win isn't a favourite victim; one loss isn't a nemesis
        let few: Vec<&Fight> = fs[6..8].iter().collect();
        assert_eq!(rivals_of(&few), (None, None));
    }
}
