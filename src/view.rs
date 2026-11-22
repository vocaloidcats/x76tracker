//! Turns tracker state into what the Slint UI shows: cards, charts, results, status lines.

use crate::tracker::{key_of, now, Core, Favorite, Game, Hit, Player, Snap};
use crate::{
    ChartData, ChartPoint, EventDetail, EventRow, FightDetail, GameData, GameRank, Opponent, PlayerCard, ProfileCard, SearchResult, StatChange,
    Tick,
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

    let mine: Vec<&crate::r5log::Fight> = if game == Game::R5 {
        core.fights.iter().filter(|f| f.me.eq_ignore_ascii_case(&fav.id)).collect()
    } else {
        Vec::new()
    };
    let show_elo = metric_is_elo(core, &key);
    let show_all = ui.show_all.contains(&key);
    let chart_data = chart(h, start, show_elo);
    let (rows, total_rows) = timeline(h, &evs, &mine, start, show_all, core.interval() as f64);

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
        total_events: total_rows as i32,
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

    let fights: Vec<&crate::r5log::Fight> = if game == Game::R5 {
        core.fights.iter().filter(|f| f.me.eq_ignore_ascii_case(fav_id) && f.t > a.t && f.t <= b.t).collect()
    } else {
        Vec::new()
    };
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
            // the opponent as they are now, from the latest ladder check
            let now_p = board.and_then(|bd| {
                bd.by_id
                    .get(&f.opponent)
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
                can_track: !f.opponent.is_empty() && !tracked.iter().any(|t| t.eq_ignore_ascii_case(&f.opponent)),
            }
        })
        .collect();

    let hint = if game != Game::R5 {
        "Who you fought is only known for R5Reloaded, from the game log.".to_string()
    } else if fight_rows.is_empty() {
        "No fights from the game log in this change. Opponents are only known for games played with the game-log patch.".to_string()
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
