//! Tracker core: downloads the x76 ladders, keeps change history for tracked players and
//! saves it to `state.json`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How this app identifies itself to x76 (User-Agent).
pub const APP_NAME: &str = "x76tracker";
const API: &str = "https://api.x76.gg/api/";
pub const INTERVALS: [u64; 4] = [30, 60, 120, 300]; // every copy downloads the full ladder: stay polite
pub const DEFAULT_INTERVAL: u64 = 60;
const MAX_HISTORY: usize = 20_000;
const SEARCH_MAX_AGE: f64 = 300.0;
const BODY_LIMIT: u64 = 64 * 1024 * 1024;

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// ------------------------------------------------------------------ games
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Game {
    Ql,
    R5,
}

impl Game {
    pub const ALL: [Game; 2] = [Game::Ql, Game::R5];
    pub fn id(self) -> &'static str {
        match self {
            Game::Ql => "ql",
            Game::R5 => "r5",
        }
    }
    pub fn from_id(s: &str) -> Option<Game> {
        Game::ALL.into_iter().find(|g| g.id() == s)
    }
    pub fn label(self) -> &'static str {
        match self {
            Game::Ql => "Quake Live",
            Game::R5 => "R5Reloaded",
        }
    }
    pub fn mode(self) -> &'static str {
        match self {
            Game::Ql => "lg duel",
            Game::R5 => "vamp 1v1 & lg duels",
        }
    }
    /// Field the "Elo" number comes from, shown in tooltips.
    pub fn elo_field(self) -> &'static str {
        match self {
            Game::Ql => "rating",
            Game::R5 => "elo",
        }
    }
    fn url(self) -> String {
        format!("{API}{}", match self { Game::Ql => "ql-leaderboard", Game::R5 => "r5-leaderboard" })
    }
}

pub fn key_of(game: &str, id: &str) -> String {
    format!("{game}:{id}")
}

// ------------------------------------------------------------------ data shapes
/// One player as the ladder shows them right now (both games normalised to this).
#[derive(Clone, Debug, Default)]
pub struct Player {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub rank: i64,
    pub points: Option<i64>,
    pub elo: Option<f64>,
    pub kills: i64,
    pub deaths: i64,
    pub tier: String,
    pub points_24h: Option<i64>,
    pub position_24h: Option<i64>,
    pub acc: Option<f64>,
}

/// A saved point in a player's history. Written whenever points, Elo, K/D or tier change.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Snap {
    pub t: f64,
    #[serde(default)]
    pub rank: Option<i64>,
    #[serde(default)]
    pub points: Option<i64>,
    #[serde(default)]
    pub elo: Option<f64>,
    #[serde(default)]
    pub kills: i64,
    #[serde(default)]
    pub deaths: i64,
    #[serde(default)]
    pub tier: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Favorite {
    pub game: String,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub added: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SavedState {
    /// Kept as a free-form map so settings written by the other version survive a round trip.
    #[serde(default)]
    pub settings: Map<String, Value>,
    #[serde(default)]
    pub favorites: Vec<Favorite>,
    #[serde(default)]
    pub history: HashMap<String, Vec<Snap>>,
}

pub struct Board {
    pub t: f64,
    pub players: Vec<Player>,
    pub by_id: HashMap<String, Vec<usize>>,
}

#[derive(Clone, Debug, Default)]
pub struct GameStatus {
    pub fetched: Option<f64>,
    pub error: Option<String>,
    pub error_at: Option<f64>,
    pub count: usize,
    pub busy: bool,
}

impl GameStatus {
    pub fn failing(&self) -> bool {
        match (self.error_at, self.fetched) {
            (Some(e), Some(f)) => e > f,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

// ------------------------------------------------------------------ normalising the two ladder formats
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => s.trim().trim_end_matches('%').replace(',', "").parse().ok().filter(|f: &f64| f.is_finite()),
        _ => None,
    }
}
fn int(v: Option<&Value>) -> Option<i64> {
    num(v).map(|f| f.round() as i64)
}
fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}
fn tier_of(p: &Map<String, Value>) -> String {
    text(p.get("rank_label")).or_else(|| text(p.get("tier"))).unwrap_or_else(|| "Nab".into())
}

fn normalise(game: Game, p: &Map<String, Value>, i: usize) -> Player {
    let aliases: Vec<String> = p
        .get("aliases")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| text(Some(v))).collect())
        .unwrap_or_default();
    let name = text(p.get("name")).or_else(|| aliases.first().cloned()).unwrap_or_else(|| "unnamed".into());
    let (id, elo, kills, deaths, acc) = match game {
        Game::Ql => (
            text(p.get("steam_id")).unwrap_or_default(),
            num(p.get("rating")),
            int(p.get("kills")).or_else(|| int(p.get("wins"))).unwrap_or(0),
            int(p.get("deaths")).or_else(|| int(p.get("losses"))).unwrap_or(0),
            None,
        ),
        Game::R5 => (
            name.clone(),
            num(p.get("elo")),
            int(p.get("kills")).unwrap_or(0),
            int(p.get("deaths")).unwrap_or(0),
            num(p.get("acc_pct")),
        ),
    };
    Player {
        id,
        name,
        aliases,
        rank: int(p.get("rank")).unwrap_or(i as i64 + 1),
        points: int(p.get("points")),
        elo,
        kills,
        deaths,
        tier: tier_of(p),
        points_24h: int(p.get("points_24h")),
        position_24h: int(p.get("position_24h")),
        acc,
    }
}

fn players_array(raw: &Value) -> Option<&Vec<Value>> {
    if let Some(a) = raw.as_array() {
        return Some(a);
    }
    ["players", "entries", "leaderboard", "results", "items", "data"]
        .iter()
        .find_map(|k| raw.get(*k).and_then(Value::as_array))
}

pub fn agent(timeout: Duration) -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // Windows' own TLS and certificate store (ureq's default root list needs a crate we don't bundle).
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(timeout))
        .user_agent(format!("{APP_NAME}/{VERSION}"))
        .build()
        .into()
}

pub fn fetch_board(game: Game) -> Result<Board, String> {
    let raw: Value = agent(Duration::from_secs(60))
        .get(game.url())
        .header("Accept", "application/json")
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_json()
        .map_err(|e| e.to_string())?;
    let players: Vec<Player> = players_array(&raw)
        .ok_or("ladder had an unexpected shape")?
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.as_object().map(|m| normalise(game, m, i)))
        .collect();
    if players.is_empty() {
        return Err("ladder returned no players".into());
    }
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in players.iter().enumerate() {
        by_id.entry(p.id.clone()).or_default().push(i);
    }
    Ok(Board { t: now(), players, by_id })
}

// ------------------------------------------------------------------ search
#[derive(Clone, Debug)]
pub struct Hit {
    pub player: Player,
    pub alias: Option<String>,
    pub tracked: bool,
}

pub fn search(board: &Board, game: Game, query: &str, tracked: &HashSet<String>, limit: usize) -> (Vec<Hit>, usize) {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return (Vec::new(), 0);
    }
    let (mut exact, mut prefix, mut contains) = (Vec::new(), Vec::new(), Vec::new());
    for p in &board.players {
        let names: Vec<String> = std::iter::once(&p.name).chain(&p.aliases).map(|n| n.to_lowercase()).collect();
        let bucket = if p.id.to_lowercase() == q || names.iter().any(|n| *n == q) {
            &mut exact
        } else if names.iter().any(|n| n.starts_with(&q)) {
            &mut prefix
        } else if names.iter().any(|n| n.contains(&q)) || (game == Game::Ql && p.id.contains(&q)) {
            &mut contains
        } else {
            continue;
        };
        let alias = if p.name.to_lowercase().contains(&q) {
            None
        } else {
            p.aliases.iter().find(|a| a.to_lowercase().contains(&q)).cloned()
        };
        bucket.push(Hit { player: p.clone(), alias, tracked: tracked.contains(&p.id) });
    }
    let total = exact.len() + prefix.len() + contains.len();
    let mut all = exact;
    all.extend(prefix);
    all.extend(contains);
    all.truncate(limit);
    (all, total)
}

// ------------------------------------------------------------------ notifications
/// Something worth a notification (only made while notifications are on in Settings).
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub title: String,
    pub body: String,
    /// A tier to show its emblem, or empty.
    pub tier: String,
    /// 1 good news, -1 bad, 0 neither.
    pub dir: i32,
}

/// Notifications from one ladder check, for your player (`me`) in one game: a tier change, the
/// best rank milestone reached, and passing (or being passed by) another tracked player.
pub fn notes_for(game: Game, me: &str, before: &HashMap<String, Player>, after: &HashMap<String, Player>) -> Vec<Note> {
    let (Some(b), Some(a)) = (before.get(me), after.get(me)) else { return Vec::new() };
    let mut out = Vec::new();
    if a.tier != b.tier && !a.tier.is_empty() && !b.tier.is_empty() {
        let up = match (a.points, b.points) {
            (Some(x), Some(y)) if x != y => x > y,
            _ => a.rank < b.rank,
        };
        out.push(Note {
            title: a.tier.clone(),
            body: format!("{} from {} · {}", if up { "Up" } else { "Down" }, b.tier, game.label()),
            tier: a.tier.clone(),
            dir: if up { 1 } else { -1 },
        });
    }
    if let Some(th) = [1, 3, 5, 10, 25, 50, 100, 250, 500, 1000].into_iter().find(|&th| b.rank > th && a.rank <= th) {
        out.push(Note {
            title: if th == 1 { "#1".into() } else { format!("Top {th}") },
            body: format!("#{} on the {} ladder", a.rank, game.label()),
            tier: a.tier.clone(),
            dir: 1,
        });
    }
    let mut others: Vec<(&String, &Player)> = before.iter().filter(|(id, _)| id.as_str() != me).collect();
    others.sort_by_key(|(_, p)| p.rank);
    for (id, ob) in others {
        let Some(oa) = after.get(id) else { continue };
        if b.rank > ob.rank && a.rank < oa.rank {
            out.push(Note { title: format!("You passed {}", oa.name), body: format!("You're #{} · {}", a.rank, game.label()), tier: String::new(), dir: 1 });
        } else if b.rank < ob.rank && a.rank > oa.rank {
            out.push(Note { title: format!("{} passed you", oa.name), body: format!("You're #{} · {}", a.rank, game.label()), tier: String::new(), dir: -1 });
        }
    }
    out
}

// ------------------------------------------------------------------ game sessions
/// One stretch of playing: from the game starting (or x76tracker noticing it) to it closing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub game: String,
    pub start: f64,
    pub end: f64,
}

const MAX_SESSIONS: usize = 500;

fn load_sessions(path: &std::path::Path) -> Vec<Session> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_sessions(path: &std::path::Path, sessions: &[Session]) {
    if let Ok(body) = serde_json::to_vec(sessions) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

// ------------------------------------------------------------------ core state
pub struct Core {
    pub state: SavedState,
    pub data_file: PathBuf,
    pub boards: HashMap<Game, Arc<Board>>,
    pub status: HashMap<Game, GameStatus>,
    /// Newest ladder entry for each tracked key (None = not on the ladder any more).
    pub latest: HashMap<String, Option<Player>>,
    pub next_poll: Option<f64>,
    /// Keys whose history grew in the last check, with the direction of the points change.
    pub fresh: HashMap<String, i32>,
    /// When each tracked key's ladder entry was last read (a snapshot is only saved on change).
    pub ladder_read: HashMap<String, f64>,
    pub load_warning: Option<String>,
    /// Your R5Reloaded fights from the game log (needs the optional game mod); kept in fights.json.
    pub fights: Vec<crate::r5log::Fight>,
    pub fights_file: PathBuf,
    /// Your Quake Live fights from the game's console log (needs `logfile` on); kept in ql-fights.json.
    pub ql_fights: Vec<crate::r5log::Fight>,
    pub ql_fights_file: PathBuf,
    /// The Quake Live console log switch as it is in autoexec.cfg (None until the game is found),
    /// and the last thing worth telling about it: (is an error, text).
    pub qllog: Option<bool>,
    pub qllog_note: Option<(bool, String)>,
    /// Linked x76 profile (public data, by handle) and its loading state.
    pub profile: Option<crate::profile::Profile>,
    pub profile_busy: bool,
    pub profile_error: Option<String>,
    /// x76's game servers (latest check) and the sampled player totals (servers.json).
    pub servers: Vec<crate::servers::Server>,
    pub servers_at: Option<f64>,
    pub servers_error: Option<String>,
    pub server_history: Vec<crate::servers::Sample>,
    pub server_file: PathBuf,
    /// Rich Presence and widget state that isn't saved (the token, what was sent, errors).
    pub discord: crate::discord::Live,
    /// The optional R5Reloaded patch (None until the game folder is known), and the last thing
    /// worth telling about it: (is an error, text).
    pub r5patch: Option<crate::r5patch::State>,
    pub r5patch_note: Option<(bool, String)>,
    /// New versions on GitHub: the last check, the download, a verified copy ready to swap in.
    pub updater: crate::update::Live,
    /// Game sessions so far (sessions.json), and the game running now with when it was noticed.
    pub sessions: Vec<Session>,
    pub sessions_file: PathBuf,
    pub playing: Option<(Game, f64)>,
    /// Notifications waiting for the app to show them.
    pub notes: Vec<Note>,
    /// Share card export: running, the last outcome (is an error, text), the last saved file.
    pub share_busy: bool,
    pub share_note: Option<(bool, String)>,
    pub share_saved: Option<PathBuf>,
}

pub struct Shared {
    pub core: Mutex<Core>,
    wake: Mutex<bool>,
    wake_cv: Condvar,
    /// One download per game at a time; a second caller waits and reuses the result.
    downloading: HashMap<Game, Mutex<()>>,
}

impl Shared {
    pub fn new(data_file: PathBuf) -> Arc<Shared> {
        let (state, load_warning) = load(&data_file);
        let fights_file = data_file.with_file_name("fights.json");
        let fights = crate::r5log::load(&fights_file);
        let ql_fights_file = data_file.with_file_name("ql-fights.json");
        let ql_fights = crate::r5log::load(&ql_fights_file);
        let sessions_file = data_file.with_file_name("sessions.json");
        let sessions = load_sessions(&sessions_file);
        let server_file = data_file.with_file_name("servers.json");
        let server_history = crate::servers::load(&server_file);
        let token = data_file.parent().and_then(crate::discord::load_token);
        Arc::new(Shared {
            core: Mutex::new(Core {
                state,
                data_file,
                boards: HashMap::new(),
                status: Game::ALL.iter().map(|g| (*g, GameStatus::default())).collect(),
                latest: HashMap::new(),
                next_poll: None,
                fresh: HashMap::new(),
                ladder_read: HashMap::new(),
                load_warning,
                fights,
                fights_file,
                ql_fights,
                ql_fights_file,
                qllog: None,
                qllog_note: None,
                sessions,
                sessions_file,
                playing: None,
                notes: Vec::new(),
                profile: None,
                profile_busy: false,
                profile_error: None,
                servers: Vec::new(),
                servers_at: None,
                servers_error: None,
                server_history,
                server_file,
                discord: crate::discord::Live { token, ..Default::default() },
                r5patch: None,
                r5patch_note: None,
                updater: crate::update::Live::default(),
                share_busy: false,
                share_note: None,
                share_saved: None,
            }),
            wake: Mutex::new(false),
            wake_cv: Condvar::new(),
            downloading: Game::ALL.iter().map(|g| (*g, Mutex::new(()))).collect(),
        })
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask the polling thread to check right away (refresh button, new interval).
    pub fn wake(&self) {
        *self.wake.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake_cv.notify_all();
    }

    fn sleep(&self, secs: u64) {
        let guard = self.wake.lock().unwrap_or_else(|e| e.into_inner());
        let (mut woke, _) = self
            .wake_cv
            .wait_timeout_while(guard, Duration::from_secs(secs), |w| !*w)
            .unwrap_or_else(|e| e.into_inner());
        *woke = false;
    }

    /// Fresh-enough ladder for search or adding a player; downloads it if needed.
    pub fn board(&self, game: Game, max_age: f64) -> Result<Arc<Board>, String> {
        let _one_at_a_time = self.downloading[&game].lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = self.lock().boards.get(&game) {
            if now() - b.t <= max_age {
                return Ok(b.clone());
            }
        }
        self.download(game)
    }

    pub fn cached_board(&self, game: Game) -> Option<Arc<Board>> {
        self.lock().boards.get(&game).filter(|b| now() - b.t <= SEARCH_MAX_AGE).cloned()
    }

    pub fn search_board(&self, game: Game) -> Result<Arc<Board>, String> {
        self.board(game, SEARCH_MAX_AGE)
    }

    fn download(&self, game: Game) -> Result<Arc<Board>, String> {
        self.lock().status.entry(game).or_default().busy = true;
        let result = fetch_board(game);
        let mut core = self.lock();
        let st = core.status.entry(game).or_default();
        st.busy = false;
        match result {
            Ok(b) => {
                st.fetched = Some(b.t);
                st.count = b.players.len();
                st.error = None;
                let b = Arc::new(b);
                core.boards.insert(game, b.clone());
                Ok(b)
            }
            Err(e) => {
                st.error = Some(e.clone());
                st.error_at = Some(now());
                Err(e)
            }
        }
    }

    /// The polling loop. Runs forever on its own thread. `notify(false)` means only the status
    /// changed (a check started); `notify(true)` means new data is in.
    pub fn run(self: Arc<Self>, notify: impl Fn(bool) + Send + 'static) {
        loop {
            self.poll_once(&notify);
            let secs = self.lock().interval();
            self.sleep(secs);
        }
    }

    pub fn poll_once(&self, notify: &dyn Fn(bool)) {
        let games: Vec<Game> = {
            let core = self.lock();
            Game::ALL.into_iter().filter(|g| core.state.favorites.iter().any(|f| f.game == g.id())).collect()
        };
        for g in games {
            self.lock().status.entry(g).or_default().busy = true;
            notify(false);
            let result = {
                let _one_at_a_time = self.downloading[&g].lock().unwrap_or_else(|e| e.into_inner());
                self.download(g)
            };
            if let Ok(board) = result {
                self.lock().apply(g, &board);
            }
        }
        let mut core = self.lock();
        core.next_poll = Some(now() + core.interval() as f64);
        drop(core);
        notify(true);
    }

    /// Checks x76's server list and records the player totals. Returns true on success.
    pub fn poll_servers(&self) -> bool {
        let result = crate::servers::fetch();
        let mut core = self.lock();
        match result {
            Ok(list) => {
                let t = now();
                let sample = crate::servers::Sample {
                    t: (t * 10.0).round() / 10.0,
                    ql: crate::servers::total(&list, crate::servers::QUAKE),
                    r5: crate::servers::total(&list, crate::servers::R5),
                };
                core.servers = list;
                core.servers_at = Some(t);
                core.servers_error = None;
                core.server_history.push(sample);
                let file = core.server_file.clone();
                crate::servers::save(&file, &mut core.server_history);
                true
            }
            Err(e) => {
                core.servers_error = Some(e);
                false
            }
        }
    }

    /// Picks up new fights from R5Reloaded's log. Returns true if there were any.
    pub fn scan_r5(&self, tail: &mut crate::r5log::Tail) -> bool {
        let (hint, recent) = {
            let core = self.lock();
            let since = now() - 3.0 * 86_400.0; // duplicates can only come from recent sessions
            let recent: Vec<_> = core.fights.iter().filter(|f| f.t >= since).cloned().collect();
            (core.setting_str("r5Path"), recent)
        };
        let new = crate::r5log::scan(tail, hint.as_deref(), &recent);
        let mut core = self.lock();
        if let Some(dir) = tail.game_dir.as_ref().map(|d| d.display().to_string()) {
            if hint.as_deref() != Some(dir.as_str()) {
                core.set_setting("r5Path", dir.into()); // remember where the game is
            }
        }
        let added = !new.is_empty();
        core.fights.extend(new);
        core.fights.sort_by(|a, b| a.t.total_cmp(&b.t));
        let rated = core.rate_fights();
        if added || rated {
            let file = core.fights_file.clone();
            crate::r5log::save(&file, &mut core.fights);
        }
        added || rated
    }

    /// Picks up new fights from Quake Live's console log. Returns true if there were any.
    pub fn scan_ql(&self, tail: &mut crate::qllog::Tail) -> bool {
        let (hint, recent) = {
            let core = self.lock();
            let since = now() - 3.0 * 86_400.0; // a log only lasts until the game restarts
            let recent: Vec<_> = core.ql_fights.iter().filter(|f| f.t >= since).cloned().collect();
            (core.setting_str("qlPath"), recent)
        };
        let new = crate::qllog::scan(tail, hint.as_deref(), &recent);
        let mut core = self.lock();
        if let Some(dir) = tail.home.as_ref().map(|d| d.display().to_string()) {
            if hint.as_deref() != Some(dir.as_str()) {
                core.set_setting("qlPath", dir.into()); // remember the account folder
            }
        }
        let added = !new.is_empty();
        core.ql_fights.extend(new);
        core.ql_fights.sort_by(|a, b| a.t.total_cmp(&b.t));
        let rated = core.rate_ql_fights();
        if added || rated {
            let file = core.ql_fights_file.clone();
            crate::r5log::save(&file, &mut core.ql_fights);
        }
        added || rated
    }

    /// Follows which game is running. When one closes after a real session (two minutes or more,
    /// or any fights), the session is saved and its index returned, for the app to show its recap.
    pub fn watch_game(&self, running: Option<Game>) -> Option<usize> {
        let mut core = self.lock();
        let t = now();
        match core.playing {
            None => {
                core.playing = running.map(|g| (g, t));
                None
            }
            Some((g, start)) if running != Some(g) => {
                core.playing = running.map(|g2| (g2, t));
                let fights = if g == Game::Ql { &core.ql_fights } else { &core.fights };
                let had_fights = fights.iter().any(|f| f.t >= start && f.t <= t);
                if t - start < 120.0 && !had_fights {
                    return None;
                }
                core.sessions.push(Session { game: g.id().into(), start, end: t });
                if core.sessions.len() > MAX_SESSIONS {
                    let extra = core.sessions.len() - MAX_SESSIONS;
                    core.sessions.drain(..extra);
                }
                save_sessions(&core.sessions_file, &core.sessions);
                if core.notify_on() {
                    let fights = if g == Game::Ql { &core.ql_fights } else { &core.fights };
                    let (w, l) = fights.iter().filter(|f| f.t >= start && f.t <= t).fold((0, 0), |(w, l), f| if f.kind == "kill" { (w + 1, l) } else { (w, l + 1) });
                    let mins = ((t - start) / 60.0).round() as i64;
                    let long = if mins < 60 { format!("{mins} min") } else { format!("{}h {}m", mins / 60, mins % 60) };
                    let record = if w + l > 0 { format!(" · {w}–{l}") } else { String::new() };
                    core.notes.push(Note {
                        title: "Session recap".into(),
                        body: format!("{} · {long}{record}", g.label()),
                        tier: String::new(),
                        dir: (w - l as i32).signum(),
                    });
                }
                Some(core.sessions.len() - 1)
            }
            _ => None,
        }
    }

    /// Reads whether autoexec.cfg turns Quake Live's console log on. Returns true if what the UI
    /// shows changed.
    pub fn check_ql_log(&self) -> bool {
        let (dir, before) = {
            let core = self.lock();
            (core.setting_str("qlPath").map(PathBuf::from), core.qllog)
        };
        let Some(dir) = dir else { return false };
        let on = crate::qllog::state(&dir);
        let mut core = self.lock();
        core.qllog = Some(on);
        before != Some(on)
    }

    /// Keeps the R5Reloaded patch the way it was set in Settings: with `fix`, puts it back when a
    /// game update replaced the patched script. Returns true if what the UI shows changed.
    pub fn check_r5_patch(&self, fix: bool) -> bool {
        use crate::r5patch::{install, state, State};
        let (dir, wanted, before) = {
            let core = self.lock();
            (core.setting_str("r5Path").map(PathBuf::from), core.state.settings.get("r5Patch").and_then(Value::as_bool), core.r5patch)
        };
        let Some(dir) = dir else { return false };
        let mut now = state(&dir);
        let mut note = None;
        if fix && wanted == Some(true) && now == State::Off {
            match install(&dir) {
                Ok(()) => {
                    now = State::On;
                    note = Some((false, "A game update had replaced the script; the patch is back on.".to_string()));
                }
                Err(e) => note = Some((true, e)),
            }
        }
        let mut core = self.lock();
        if wanted.is_none() && now == State::On {
            core.set_setting("r5Patch", true.into()); // patched before this switch existed
        }
        core.r5patch = Some(now);
        let changed = before != Some(now) || note.is_some();
        if note.is_some() {
            core.r5patch_note = note;
        }
        changed
    }

    pub fn add_favorite(&self, game: Game, id: &str) -> Result<(), String> {
        let board = self.board(game, SEARCH_MAX_AGE)?;
        let mut core = self.lock();
        if core.state.favorites.iter().any(|f| f.game == game.id() && f.id == id) {
            return Ok(());
        }
        let Some(&i) = board.by_id.get(id).and_then(|v| v.first()) else {
            return Err("That player isn't on the ladder any more.".into());
        };
        core.state.favorites.push(Favorite {
            game: game.id().into(),
            id: id.into(),
            name: board.players[i].name.clone(),
            added: (now() * 10.0).round() / 10.0,
        });
        let fav = core.state.favorites.last().unwrap().clone();
        core.record(game, &fav, &board, now());
        core.save()
    }
}

impl Core {
    pub fn interval(&self) -> u64 {
        self.state
            .settings
            .get("interval")
            .and_then(Value::as_u64)
            .filter(|v| INTERVALS.contains(v))
            .unwrap_or(DEFAULT_INTERVAL)
    }

    pub fn setting_str(&self, key: &str) -> Option<String> {
        self.state.settings.get(key).and_then(Value::as_str).map(str::to_string)
    }

    pub fn set_setting(&mut self, key: &str, value: Value) {
        self.state.settings.insert(key.into(), value);
        let _ = self.save();
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.data_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.data_file.with_extension("tmp");
        let body = serde_json::to_vec(&self.state).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.data_file).map_err(|e| e.to_string())
    }

    fn find<'b>(&self, game: Game, fav: &Favorite, board: &'b Board) -> Option<&'b Player> {
        let cands: Vec<&Player> = board.by_id.get(&fav.id)?.iter().map(|&i| &board.players[i]).collect();
        if cands.len() <= 1 {
            return cands.first().copied();
        }
        // R5 is keyed by name and a few names are shared: follow the entry whose
        // game count continues the one we were tracking.
        let Some(last) = self.state.history.get(&key_of(game.id(), &fav.id)).and_then(|h| h.last()) else {
            return cands.first().copied();
        };
        let prev = last.kills + last.deaths;
        cands
            .into_iter()
            .min_by_key(|p| {
                let d = p.kills + p.deaths - prev;
                (if d >= 0 { 0 } else { 1 }, d.abs(), p.rank)
            })
    }

    /// Stores the newest numbers for one favourite. Returns true if its history grew.
    fn record(&mut self, game: Game, fav: &Favorite, board: &Board, t: f64) -> bool {
        let key = key_of(game.id(), &fav.id);
        let found = self.find(game, fav, board).cloned();
        self.latest.insert(key.clone(), found.clone());
        let Some(p) = found else { return false };
        self.ladder_read.insert(key.clone(), t);
        if let Some(f) = self.state.favorites.iter_mut().find(|f| f.game == fav.game && f.id == fav.id) {
            if f.name != p.name {
                f.name = p.name.clone();
            }
        }
        let snap = Snap {
            t: (t * 10.0).round() / 10.0,
            rank: Some(p.rank),
            points: p.points,
            elo: p.elo,
            kills: p.kills,
            deaths: p.deaths,
            tier: p.tier.clone(),
        };
        let hist = self.state.history.entry(key.clone()).or_default();
        if let Some(last) = hist.last_mut() {
            let same = last.points == snap.points
                && last.elo == snap.elo
                && last.kills == snap.kills
                && last.deaths == snap.deaths
                && last.tier == snap.tier;
            if same {
                last.rank = snap.rank; // keep the newest rank without adding a point
                return false;
            }
            let dir = match (snap.points, last.points) {
                (Some(a), Some(b)) if a != b => (a - b).signum() as i32,
                _ => match (snap.elo, last.elo) {
                    (Some(a), Some(b)) if a != b => if a > b { 1 } else { -1 },
                    _ => 0,
                },
            };
            self.fresh.insert(key, dir);
        }
        hist.push(snap);
        if hist.len() > MAX_HISTORY {
            let extra = hist.len() - MAX_HISTORY;
            hist.drain(..extra);
        }
        true
    }

    pub fn notify_on(&self) -> bool {
        self.state.settings.get("notify").and_then(Value::as_bool).unwrap_or(false)
    }

    fn apply(&mut self, game: Game, board: &Board) {
        let favs: Vec<Favorite> = self.state.favorites.iter().filter(|f| f.game == game.id()).cloned().collect();
        let standing = |core: &Core| -> HashMap<String, Player> {
            favs.iter().filter_map(|f| core.latest.get(&key_of(game.id(), &f.id)).cloned().flatten().map(|p| (f.id.clone(), p))).collect()
        };
        let before = standing(self);
        let mut changed = false;
        for fav in &favs {
            changed |= self.record(game, fav, board, board.t);
        }
        if self.notify_on() {
            if let Some(me) = crate::discord::Config::load(self).player(self, game) {
                let after = standing(self);
                let notes = notes_for(game, &me, &before, &after);
                self.notes.extend(notes);
            }
        }
        if changed {
            let _ = self.save();
        }
    }

    /// Fills in the opponent's (and your) Elo, points, tier and rank for recent fights, from the
    /// last R5 ladder check. Fights are read within seconds, so that check is from before the fight.
    /// The same for Quake Live, whose console log has names: an opponent is found by their ladder
    /// name, then by an older name (colour codes don't count).
    fn rate_ql_fights(&mut self) -> bool {
        let Some(board) = self.boards.get(&Game::Ql).cloned() else { return false };
        let plain = |n: &str| crate::qllog::plain(n).to_lowercase();
        let mut by_name: HashMap<String, usize> = HashMap::new();
        let mut by_alias: HashMap<String, usize> = HashMap::new();
        // the ladder is best first, so where two players share a name the higher one wins
        for (i, p) in board.players.iter().enumerate() {
            by_name.entry(plain(&p.name)).or_insert(i);
            for a in &p.aliases {
                by_alias.entry(plain(a)).or_insert(i);
            }
        }
        let cutoff = now() - 1800.0;
        let mut changed = false;
        for f in self.ql_fights.iter_mut().filter(|f| f.opp_elo.is_none() && f.t >= cutoff && !f.opponent.is_empty()) {
            let key = plain(&f.opponent);
            if let Some(&i) = by_name.get(&key).or_else(|| by_alias.get(&key)) {
                let o = &board.players[i];
                f.opp_id = Some(o.id.clone());
                f.opp_elo = o.elo;
                f.opp_points = o.points;
                f.opp_tier = Some(o.tier.clone());
                f.opp_rank = Some(o.rank);
                f.my_elo = board.by_id.get(&f.me).and_then(|v| v.first()).and_then(|&j| board.players[j].elo);
                changed = true;
            }
        }
        changed
    }

    fn rate_fights(&mut self) -> bool {
        let Some(board) = self.boards.get(&Game::R5).cloned() else { return false };
        let find = |name: &str| -> Option<&Player> {
            board
                .by_id
                .get(name)
                .and_then(|v| v.first())
                .map(|&i| &board.players[i])
                .or_else(|| board.players.iter().find(|p| p.name.eq_ignore_ascii_case(name)))
        };
        let cutoff = now() - 1800.0; // older fights would get today's numbers, not the ones at the time
        let mut changed = false;
        for f in self.fights.iter_mut().filter(|f| f.opp_elo.is_none() && f.t >= cutoff && !f.opponent.is_empty()) {
            if let Some(o) = find(&f.opponent) {
                f.opp_elo = o.elo;
                f.opp_points = o.points;
                f.opp_tier = Some(o.tier.clone());
                f.opp_rank = Some(o.rank);
                f.my_elo = find(&f.me).and_then(|p| p.elo);
                changed = true;
            }
        }
        changed
    }

    pub fn remove_favorite(&mut self, game: &str, id: &str) {
        self.state.favorites.retain(|f| !(f.game == game && f.id == id));
        self.latest.remove(&key_of(game, id));
        // history stays on disk, so adding the player again later continues their chart
        let _ = self.save();
    }

    pub fn move_favorite(&mut self, game: &str, id: &str, dir: i32) {
        let favs = &mut self.state.favorites;
        let same: Vec<usize> = (0..favs.len()).filter(|&i| favs[i].game == game).collect();
        let Some(pos) = same.iter().position(|&i| favs[i].id == id) else { return };
        let other = pos as i64 + if dir > 0 { 1 } else { -1 };
        if other >= 0 && (other as usize) < same.len() {
            favs.swap(same[pos], same[other as usize]);
            let _ = self.save();
        }
    }
}

fn load(path: &PathBuf) -> (SavedState, Option<String>) {
    let Ok(bytes) = std::fs::read(path) else {
        return (SavedState::default(), None);
    };
    match serde_json::from_slice::<SavedState>(&bytes) {
        Ok(mut s) => {
            s.favorites.retain(|f| Game::from_id(&f.game).is_some() && !f.id.is_empty());
            (s, None)
        }
        Err(e) => {
            let backup = path.with_extension(format!("broken-{}.json", now() as u64));
            let _ = std::fs::rename(path, &backup);
            let name = backup.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            (SavedState::default(), Some(format!("Couldn't read the saved history ({e}). It was moved to {name} and a fresh one started.")))
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn sessions_start_and_end_with_the_game() {
        let dir = std::env::temp_dir().join(format!("x76-sessions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = Shared::new(dir.join("state.json"));
        assert_eq!(shared.watch_game(Some(Game::Ql)), None); // the game starts
        shared.lock().playing = Some((Game::Ql, now() - 300.0)); // ...five minutes ago
        assert_eq!(shared.watch_game(Some(Game::Ql)), None); // still playing
        assert_eq!(shared.watch_game(None), Some(0)); // it closes: a session
        assert_eq!(load_sessions(&dir.join("sessions.json")).len(), 1);
        // a game open for a few seconds with no fights isn't a session
        assert_eq!(shared.watch_game(Some(Game::R5)), None);
        assert_eq!(shared.watch_game(None), None);
        assert_eq!(shared.lock().sessions.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod note_tests {
    use super::*;

    fn player(name: &str, rank: i64, points: i64, tier: &str) -> Player {
        Player { id: name.into(), name: name.into(), rank, points: Some(points), tier: tier.into(), ..Default::default() }
    }

    #[test]
    fn notes_for_tiers_milestones_and_passing() {
        let before: HashMap<String, Player> = [("me", player("me", 104, 9900, "Diamond III")), ("pal", player("pal", 101, 9950, "Diamond III"))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let after: HashMap<String, Player> = [("me", player("me", 99, 10010, "Diamond II")), ("pal", player("pal", 102, 9950, "Diamond III"))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let notes = notes_for(Game::Ql, "me", &before, &after);
        let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["Diamond II", "Top 100", "You passed pal"]);
        assert_eq!(notes[0].body, "Up from Diamond III · Quake Live");
        // and the other way round
        let back = notes_for(Game::Ql, "me", &after, &before);
        let titles: Vec<&str> = back.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["Diamond III", "pal passed you"]);
        assert!(notes_for(Game::Ql, "nobody", &before, &after).is_empty());
    }
}
