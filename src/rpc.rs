//! Discord Rich Presence over Discord's local pipe (no login, no token). While R5Reloaded or
//! Quake Live runs, your Discord status shows your tier, rank, Elo and your last 48 hours.
//!
//! It runs under x76tracker's own Discord app, or under your widget's app (Settings) so the
//! widget's "while playing" panel shows next to it.

use crate::discord::{emblem_url, recent, summary, valid_id, Config};
use crate::tracker::{now, Core, Game, Shared};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

/// Executable names of the games, lower case.
const R5_EXE: &str = "r5apex.exe";
const QL_EXES: [&str; 2] = ["quakelive_steam.exe", "quakelive.exe"];
const RETRY_SECS: f64 = 15.0;

// ------------------------------------------------------------------ the pipe
struct Conn {
    pipe: std::fs::File,
    app: String,
    nonce: u64,
}

impl Conn {
    /// Connects and says hello as `app`. Returns the connection and the Discord user (id, name).
    fn open(app: &str) -> Result<(Conn, Option<(String, String)>), String> {
        let mut last = "Discord isn't running on this PC.".to_string();
        for i in 0..10 {
            let path = format!(r"\\.\pipe\discord-ipc-{i}");
            let Ok(pipe) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else { continue };
            let mut c = Conn { pipe, app: app.to_string(), nonce: 0 };
            if let Err(e) = c.send(0, &json!({"v": 1, "client_id": app})) {
                last = e.to_string();
                continue;
            }
            match c.recv() {
                Ok((1, v)) if v.get("evt").and_then(Value::as_str) == Some("READY") => {
                    let user = v.pointer("/data/user").map(|u| {
                        let id = u.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                        let name = u
                            .get("global_name")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .or_else(|| u.get("username").and_then(Value::as_str))
                            .unwrap_or("")
                            .to_string();
                        (id, name)
                    });
                    return Ok((c, user));
                }
                Ok((2, v)) => {
                    let msg = v.get("message").and_then(Value::as_str).unwrap_or("closed the connection");
                    return Err(format!("Discord said: {msg} (app {app})."));
                }
                Ok(_) => last = "Discord sent an unexpected hello.".into(),
                Err(e) => last = e.to_string(),
            }
        }
        Err(last)
    }

    fn send(&mut self, op: u32, v: &Value) -> std::io::Result<()> {
        let body = serde_json::to_vec(v)?;
        let mut frame = Vec::with_capacity(body.len() + 8);
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        self.pipe.write_all(&frame)
    }

    fn recv(&mut self) -> std::io::Result<(u32, Value)> {
        let mut head = [0u8; 8];
        self.pipe.read_exact(&mut head)?;
        let op = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        if len > 1 << 20 {
            return Err(std::io::Error::other("oversized message from Discord"));
        }
        let mut body = vec![0u8; len];
        self.pipe.read_exact(&mut body)?;
        Ok((op, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    /// Sets the activity (None clears it). Err(true) = connection lost; Err(false) = Discord refused.
    fn set_activity(&mut self, activity: Option<&Value>) -> Result<(), (bool, String)> {
        self.nonce += 1;
        let nonce = format!("x76tracker-{}-{}", std::process::id(), self.nonce);
        let mut args = json!({"pid": std::process::id()});
        if let Some(a) = activity {
            args["activity"] = a.clone();
        }
        self.send(1, &json!({"cmd": "SET_ACTIVITY", "args": args, "nonce": nonce})).map_err(|e| (true, e.to_string()))?;
        loop {
            let (op, v) = self.recv().map_err(|e| (true, e.to_string()))?;
            match op {
                1 if v.get("nonce").and_then(Value::as_str) == Some(nonce.as_str()) => {
                    if v.get("evt").and_then(Value::as_str) == Some("ERROR") {
                        let msg = v.pointer("/data/message").and_then(Value::as_str).unwrap_or("unknown error");
                        return Err((false, format!("Discord refused the presence: {msg}")));
                    }
                    return Ok(());
                }
                2 => return Err((true, "Discord closed the connection.".into())),
                3 => {
                    let _ = self.send(4, &v); // ping → pong
                }
                _ => {}
            }
        }
    }
}

// ------------------------------------------------------------------ which game is running
#[cfg(windows)]
fn running() -> Vec<String> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Entry {
        size: u32,
        usage: u32,
        pid: u32,
        heap: usize,
        module: u32,
        threads: u32,
        parent: u32,
        priority: i32,
        flags: u32,
        exe: [u16; 260],
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Process32FirstW(snap: *mut c_void, entry: *mut Entry) -> i32;
        fn Process32NextW(snap: *mut c_void, entry: *mut Entry) -> i32;
        fn CloseHandle(h: *mut c_void) -> i32;
    }
    let mut names = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(0x2, 0); // TH32CS_SNAPPROCESS
        if snap.is_null() || snap as isize == -1 {
            return names;
        }
        let mut e: Entry = std::mem::zeroed();
        e.size = std::mem::size_of::<Entry>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            let len = e.exe.iter().position(|&c| c == 0).unwrap_or(e.exe.len());
            names.push(String::from_utf16_lossy(&e.exe[..len]).to_lowercase());
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    names
}

#[cfg(not(windows))]
fn running() -> Vec<String> {
    Vec::new()
}

fn game_running() -> Option<Game> {
    let names = running();
    if names.iter().any(|n| n == R5_EXE) {
        Some(Game::R5)
    } else if names.iter().any(|n| QL_EXES.contains(&n.as_str())) {
        Some(Game::Ql)
    } else {
        None
    }
}

// ------------------------------------------------------------------ what to show
fn activity(core: &Core, cfg: &Config, game: Game, since: f64) -> Value {
    let stats = cfg.player(core, game).and_then(|id| crate::view::player_stats(core, game, &id));
    let (details, state, tier) = match &stats {
        Some(s) => {
            let line = summary(s);
            let played = s.recent_wins + s.recent_losses > 0 || s.recent_elo.is_some_and(|d| d.abs() >= 0.05);
            let state = if played {
                format!("48h: {}", recent(s))
            } else {
                s.points.map(|p| format!("{} points", crate::view::thousands(p))).unwrap_or_default()
            };
            let details = if line.is_empty() { game.label().to_string() } else { format!("{} · {line}", game.label()) };
            (details, state, s.tier.clone())
        }
        None => (game.label().to_string(), "Not tracking a player yet".to_string(), String::new()),
    };
    let button = match core.profile.as_ref() {
        Some(p) => json!({"label": "x76 profile", "url": format!("https://x76.gg/u/?h={}", p.handle)}),
        None => json!({"label": "x76 ladder", "url": "https://x76.gg/leaderboard/"}),
    };
    let mut a = json!({
        "type": 0,
        "details": clip(&details),
        "timestamps": {"start": since as i64},
        "assets": {
            "large_image": emblem_url(&tier),
            "large_text": clip(&if tier.is_empty() { game.label().to_string() } else { format!("{} · {tier}", game.label()) }),
        },
        "buttons": [button],
    });
    if state.chars().count() >= 2 {
        a["state"] = clip(&state).into();
    }
    a
}

fn clip(s: &str) -> String {
    if s.chars().count() <= 128 { s.to_string() } else { s.chars().take(127).collect::<String>() + "…" }
}

// ------------------------------------------------------------------ the loop
/// Runs forever on its own thread. `notify` is called when something the UI shows changed.
pub fn run(shared: Arc<Shared>, notify: impl Fn()) {
    let mut conn: Option<Conn> = None;
    let mut shown: Option<Value> = None;
    let mut last_try = f64::NEG_INFINITY;
    let mut was = (None, false, None::<String>);
    loop {
        let game = game_running();
        let (cfg, wanted) = {
            let mut core = shared.lock();
            if core.discord.playing != game {
                core.discord.playing = game;
                core.discord.playing_since = now();
            }
            let cfg = Config::load(&core);
            let since = core.discord.playing_since;
            let wanted = if cfg.rpc { game.map(|g| activity(&core, &cfg, g, since)) } else { None };
            (cfg, wanted)
        };
        let app = cfg.rpc_app();
        // the widget needs your Discord user ID: Discord tells us when we say hello
        let need_user = valid_id(&cfg.app_id) && !valid_id(&cfg.user_id);

        if conn.as_ref().is_some_and(|c| c.app != app) {
            conn = None; // different app: start over (dropping the pipe ends the old presence)
            shown = None;
        }
        let mut error = None;
        if (wanted.is_some() || need_user) && conn.is_none() && now() - last_try >= RETRY_SECS {
            last_try = now();
            match Conn::open(&app) {
                Ok((c, user)) => {
                    conn = Some(c);
                    if let Some((id, name)) = user {
                        let mut core = shared.lock();
                        core.discord.rpc_user = Some(name).filter(|n| !n.is_empty());
                        let mut cfg = Config::load(&core);
                        if !valid_id(&cfg.user_id) && valid_id(&id) {
                            cfg.user_id = id;
                            cfg.save(&mut core);
                        }
                    }
                }
                Err(e) => error = Some(e),
            }
        }
        if let Some(c) = conn.as_mut() {
            match &wanted {
                Some(a) if shown.as_ref() != Some(a) => match c.set_activity(Some(a)) {
                    Ok(()) => shown = Some(a.clone()),
                    Err((lost, e)) => {
                        error = Some(e);
                        if lost {
                            conn = None;
                            shown = None;
                        } else {
                            shown = Some(a.clone()); // don't resend what Discord refused
                        }
                    }
                },
                Some(_) => {}
                None => {
                    if shown.is_some() {
                        let _ = c.set_activity(None);
                    }
                    conn = None; // nothing to show: hang up
                    shown = None;
                }
            }
        } else if wanted.is_none() {
            shown = None;
        }

        let showing = shown.is_some() && conn.is_some();
        {
            let mut core = shared.lock();
            core.discord.rpc_showing = showing;
            if error.is_some() || !cfg.rpc || wanted.is_none() || showing {
                core.discord.rpc_error = error.clone();
            }
        }
        let now_state = (game, showing, error);
        if now_state != was {
            was = now_state;
            notify();
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Says hello to the Discord app on this PC and hangs up (sets no activity).
    /// `cargo test --release rpc -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn hello() {
        match Conn::open(crate::discord::APP_ID) {
            Ok((_, user)) => println!("connected; user known: {}", user.is_some_and(|(id, _)| valid_id(&id))),
            Err(e) => println!("no connection: {e}"),
        }
    }

    #[test]
    fn sees_processes() {
        assert!(running().iter().any(|n| n.ends_with(".exe")));
    }
}
