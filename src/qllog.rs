//! Your Quake Live fights, from the game's own console log.
//!
//! Quake Live writes everything its console shows to `baseq3/qconsole.log` when the `logfile` cvar
//! is on. x76's LG arena prints a line for every kill, and after each fight you were in, both
//! players' accuracy in it:
//!
//!   ^5something has to change^7^7 was electrocuted by ^5am9u^7^7
//!   ^7Enemy: ^147％
//!   ^7You: ^340％
//!
//! The log is per Steam account (`Quake Live/<SteamID>/baseq3`), so the folder tells whose fights
//! they are, and that SteamID is the player's id on x76's Quake Live ladder. Lines carry no time:
//! the log is written as things happen and read every couple of seconds, so a line's time is
//! placed between the last read and the file's last change by where it is in what was new (the
//! first read starts from the session's first line, "logfile opened on …"). Fights are told apart
//! by where they are in the log, so reading a log twice doesn't count them twice.
//!
//! Settings can switch the log on: one line in the account's `autoexec.cfg` (`set logfile 2`,
//! under a comment naming x76tracker). Switching it off takes exactly those lines out.

use crate::r5log::Fight;
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const LOG: &str = "qconsole.log";
const AUTOEXEC: &str = "autoexec.cfg";
const BACKUP_SUFFIX: &str = ".bak-x76tracker";
const MARK: &str = "// x76tracker: write the console to qconsole.log, so x76tracker can show your Quake Live fights";
const SET_LINE: &str = "set logfile 2";
/// A fight waits this long for its accuracy lines before it's handed on without them.
const ACC_WAIT: f64 = 4.0;

/// Takes out Quake Live's colour codes (^0 … ^9).
pub fn plain(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '^' && chars.peek().is_some_and(|n| n.is_ascii_digit()) {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

fn is_steam_id(s: &str) -> bool {
    s.len() == 17 && s.starts_with("7656119") && s.bytes().all(|b| b.is_ascii_digit())
}

// ------------------------------------------------------------------ finding the game
/// The SteamID an account folder belongs to (`…/Quake Live/<SteamID>/baseq3`).
pub fn account_of(baseq3: &Path) -> Option<String> {
    let id = baseq3.parent()?.file_name()?.to_str()?;
    is_steam_id(id).then(|| id.to_string())
}

/// Steam library folders: the default Steam folder's list, plus the usual spots on every drive.
fn steam_libraries() -> Vec<PathBuf> {
    let mut libs = Vec::new();
    for steam in ["C:\\Program Files (x86)\\Steam", "C:\\Program Files\\Steam"] {
        let steam = PathBuf::from(steam);
        if let Ok(text) = std::fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")) {
            for line in text.lines() {
                let parts: Vec<&str> = line.split('"').collect();
                // "path"		"L:\\SteamLibrary"
                if parts.len() >= 4 && parts[1] == "path" {
                    libs.push(PathBuf::from(parts[3].replace("\\\\", "\\")));
                }
            }
        }
        libs.push(steam);
    }
    for letter in 'C'..='Z' {
        for base in ["SteamLibrary", "Steam", "Games\\SteamLibrary", "Program Files (x86)\\Steam"] {
            libs.push(PathBuf::from(format!("{letter}:\\{base}")));
        }
    }
    libs
}

/// Every Quake Live account folder (`baseq3` inside `<SteamID>`) on this PC.
fn account_dirs(hint: Option<&str>) -> Vec<PathBuf> {
    let mut games: Vec<PathBuf> = steam_libraries().into_iter().map(|l| l.join("steamapps").join("common").join("Quake Live")).collect();
    // the x76 client runs its own copy of the game, with links to the Steam account folders
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        games.push(PathBuf::from(local).join("x76").join("client").join("ql-4gb"));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    if let Some(h) = hint.map(PathBuf::from).filter(|p| p.is_dir() && account_of(p).is_some()) {
        found.push(h);
    }
    for game in games {
        let Ok(entries) = std::fs::read_dir(&game) else { continue };
        for e in entries.flatten() {
            let dir = e.path().join("baseq3");
            if is_steam_id(&e.file_name().to_string_lossy()) && dir.is_dir() {
                found.push(dir);
            }
        }
    }
    // the same folder reached two ways (the x76 client's links) counts once
    let mut seen = HashSet::new();
    found.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
    found
}

fn modified(p: &Path) -> Option<std::time::SystemTime> {
    p.metadata().and_then(|m| m.modified()).ok()
}

/// The account folder played most recently: the newest console log, else the newest config.
pub fn find_home(hint: Option<&str>) -> Option<PathBuf> {
    account_dirs(hint).into_iter().max_by_key(|d| (modified(&d.join(LOG)), modified(&d.join("qzconfig.cfg"))))
}

/// Your in-game name, from the account's config (`seta name "…"`), without colour codes.
fn my_name(baseq3: &Path) -> Option<String> {
    let text = std::fs::read(baseq3.join("qzconfig.cfg")).ok()?;
    let text = String::from_utf8_lossy(&text);
    text.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("seta name ")?;
        Some(plain(rest.trim().trim_matches('"')))
    })
}

// ------------------------------------------------------------------ the switch (autoexec.cfg)
/// Whether autoexec.cfg turns the console log on (our line, or the player's own).
pub fn is_on(text: &str) -> bool {
    text.lines().any(|l| {
        let words: Vec<String> = l.split("//").next().unwrap_or("").split_whitespace().map(|w| w.trim_matches('"').to_ascii_lowercase()).collect();
        words.len() >= 3 && (words[0] == "set" || words[0] == "seta") && words[1] == "logfile" && matches!(words[2].as_str(), "1" | "2" | "3")
    })
}

/// Whether autoexec.cfg has x76tracker's own lines (the ones switching off would take out).
#[cfg(test)]
fn has_ours(text: &str) -> bool {
    text.lines().any(|l| l.trim() == MARK)
}

pub fn add(text: &str) -> String {
    if is_on(text) {
        return text.to_string();
    }
    let eol = if text.contains("\r\n") || text.is_empty() { "\r\n" } else { "\n" };
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(&format!("{MARK}{eol}{SET_LINE}{eol}"));
    out
}

/// Takes out our comment and the `set logfile 2` right after it; everything else stays as it was.
pub fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut skip_set = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if t == MARK {
            skip_set = true;
            continue;
        }
        if skip_set && t == SET_LINE {
            skip_set = false;
            continue;
        }
        skip_set = false;
        out.push_str(line);
    }
    out
}

/// The switch as it is on disk.
pub fn state(baseq3: &Path) -> bool {
    std::fs::read(baseq3.join(AUTOEXEC)).map(|b| is_on(&String::from_utf8_lossy(&b))).unwrap_or(false)
}

fn write(baseq3: &Path, f: impl Fn(&str) -> String) -> Result<(), String> {
    let path = baseq3.join(AUTOEXEC);
    // Quake Live reads its configs as plain bytes; keep them exactly as they were (latin-1 round trip)
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("Couldn't read {}: {e}", path.display())),
    };
    if bytes.len() >= 2 && (bytes[..2] == [0xff, 0xfe] || bytes[..2] == [0xfe, 0xff]) {
        return Err("autoexec.cfg is saved as UTF-16, which Quake Live can't read. Save it as ANSI or UTF-8 first.".into());
    }
    let text: String = bytes.iter().map(|&b| b as char).collect();
    let new = f(&text);
    if new == text {
        return Ok(());
    }
    let backup = path.with_file_name(format!("{AUTOEXEC}{BACKUP_SUFFIX}"));
    if !bytes.is_empty() && !backup.exists() {
        std::fs::copy(&path, &backup).map_err(|e| format!("Couldn't back up autoexec.cfg: {e}"))?;
    }
    let out: Vec<u8> = new.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect();
    let tmp = path.with_extension("cfg.tmp");
    std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

pub fn install(baseq3: &Path) -> Result<(), String> {
    write(baseq3, add)
}

pub fn remove(baseq3: &Path) -> Result<(), String> {
    write(baseq3, strip)?;
    if state(baseq3) {
        return Err("autoexec.cfg has its own logfile line, which x76tracker leaves alone. Take it out there to switch the log off.".into());
    }
    Ok(())
}

// ------------------------------------------------------------------ reading the log
/// An obituary: (victim, killer), both as written (with colour codes).
fn obituary(line: &str) -> Option<(&str, &str)> {
    // "<victim>^7^7 was <how> by <killer>^7^7"; chat lines are "<name>^7^7: …", so a chat message
    // can't pass for one (its "victim" would hold the "^7^7: ")
    let (victim, rest) = line.split_once("^7^7 was ")?;
    if victim.contains("^7^7: ") {
        return None;
    }
    let (how, killer) = rest.split_once(" by ")?;
    if how.is_empty() || !how.chars().all(|c| c.is_ascii_lowercase() || c == ' ') {
        return None;
    }
    let killer = killer.trim_end().strip_suffix("^7^7")?;
    Some((victim, killer))
}

/// "^7Enemy: ^147％" → ("enemy", 47.0); "^7You: ^340％" → ("you", 40.0).
fn accuracy(line: &str) -> Option<(&'static str, f64)> {
    let p = plain(line);
    let (who, rest) = if let Some(r) = p.strip_prefix("Enemy:") {
        ("enemy", r)
    } else if let Some(r) = p.strip_prefix("You:") {
        ("you", r)
    } else {
        return None;
    };
    let digits: String = rest.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let after = rest.trim()[digits.len()..].trim();
    if !(after == "%" || after == "％") {
        return None;
    }
    digits.parse().ok().map(|v| (who, v))
}

/// Where we are in the console log.
#[derive(Default)]
pub struct Tail {
    pub home: Option<PathBuf>,
    header: String,
    pos: u64,
    /// When the log had been read up to `pos`.
    read_t: f64,
    last_find: f64,
    /// A fight read but still waiting for its accuracy lines, and when it was read.
    pending: Option<(Fight, f64)>,
}

/// Reads anything new in the console log. Returns the fights not seen before.
pub fn scan(tail: &mut Tail, hint: Option<&str>, known: &[Fight]) -> Vec<Fight> {
    let now = crate::tracker::now();
    if now - tail.last_find > if tail.home.is_some() { 600.0 } else { 60.0 } {
        tail.last_find = now;
        let found = find_home(hint);
        if found.is_some() && found != tail.home {
            tail.home = found;
            tail.header.clear(); // another account: read its log from the start
        }
    }
    let mut out = Vec::new();
    let Some(home) = tail.home.clone() else { return out };
    let seen: HashSet<&str> = known.iter().filter_map(|f| f.src.as_deref()).collect();
    let emit = |f: Fight, out: &mut Vec<Fight>| {
        if !f.src.as_deref().is_some_and(|s| seen.contains(s)) {
            out.push(f);
        }
    };
    read_new(tail, &home, now, &mut |f| emit(f, &mut out));
    if let Some((_, read_at)) = &tail.pending {
        if now - read_at > ACC_WAIT {
            let (f, _) = tail.pending.take().unwrap();
            emit(f, &mut out);
        }
    }
    out
}

fn read_new(tail: &mut Tail, home: &Path, now: f64, emit: &mut dyn FnMut(Fight)) {
    let (Some(me_id), log) = (account_of(home), home.join(LOG)) else { return };
    let Ok(mut file) = std::fs::File::open(&log) else { return };
    // the first line names the session ("logfile opened on Sun Nov 22 13:39:53 2026"); Quake Live
    // starts the file over each time it starts
    let mut head = [0u8; 80];
    let n = file.read(&mut head).unwrap_or(0);
    let header = String::from_utf8_lossy(&head[..n]).lines().next().unwrap_or("").trim().to_string();
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if header != tail.header || len < tail.pos {
        tail.header = header.clone();
        tail.pos = 0;
        tail.pending = None;
    }
    if len == tail.pos || file.seek(SeekFrom::Start(tail.pos)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return;
    }
    // only whole lines; a half-written last line is read next time
    let complete = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    if complete == 0 {
        return;
    }
    // no times in the log: what's new was written between the last read (or the session's start)
    // and the file's last change, at a pace taken as even
    let end_t = modified(&log)
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(now)
        .min(now);
    let from_t = if tail.pos == 0 { session_start(&tail.header) } else { Some(tail.read_t) }
        .filter(|t| *t > 0.0 && *t <= end_t)
        .unwrap_or(end_t);
    let me = my_name(home).unwrap_or_default();
    let start = tail.pos;
    let mut offset = 0usize;
    for raw in buf[..complete].split_inclusive(|&b| b == b'\n') {
        let at = start + offset as u64;
        offset += raw.len();
        let t = from_t + (end_t - from_t) * offset as f64 / complete as f64;
        let line = String::from_utf8_lossy(raw);
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some((who, v)) = accuracy(line) {
            if let Some((f, _)) = tail.pending.as_mut() {
                if who == "you" {
                    f.my_acc = Some(v);
                } else {
                    f.opp_acc = Some(v);
                }
                if f.my_acc.is_some() && f.opp_acc.is_some() {
                    emit(tail.pending.take().unwrap().0);
                }
            }
            continue;
        }
        // any other line ends the wait for accuracy lines
        if let Some((f, _)) = tail.pending.take() {
            emit(f);
        }
        let Some((victim, killer)) = obituary(line) else { continue };
        let (victim, killer) = (plain(victim), plain(killer));
        let kind = if !me.is_empty() && victim.eq_ignore_ascii_case(&me) {
            "death"
        } else if !me.is_empty() && killer.eq_ignore_ascii_case(&me) {
            "kill"
        } else {
            continue; // someone else's fight
        };
        let opponent = if kind == "kill" { victim } else { killer };
        let fight = Fight {
            t,
            kind: kind.into(),
            me: me_id.clone(),
            opponent,
            src: Some(format!("{}@{at}", tail.header)),
            ..Default::default()
        };
        tail.pending = Some((fight, now));
    }
    tail.pos += complete as u64;
    tail.read_t = end_t;
}

/// When the log was started: "logfile opened on Sun Nov 22 13:39:53 2026" (local time).
fn session_start(header: &str) -> Option<f64> {
    use chrono::{Local, NaiveDateTime, TimeZone};
    let text = header.strip_prefix("logfile opened on ")?.split_whitespace().collect::<Vec<_>>().join(" ");
    let when = NaiveDateTime::parse_from_str(&text, "%a %b %d %H:%M:%S %Y").ok()?;
    Some(Local.from_local_datetime(&when).earliest()?.timestamp() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_lines() {
        assert_eq!(plain("^5something has to change^7^7"), "something has to change");
        assert_eq!(plain("a^b"), "a^b");
        let l = "^5something has to change^7^7 was electrocuted by ^5am9u^7^7";
        assert_eq!(obituary(l), Some(("^5something has to change", "^5am9u")));
        assert_eq!(obituary("^5BIG DOOR prime^7^7: ^2pain"), None);
        assert_eq!(obituary("^5x^7^7: ^2^5a^7^7 was electrocuted by ^5b^7^7"), None);
        assert_eq!(accuracy("^7Enemy: ^147％"), Some(("enemy", 47.0)));
        assert_eq!(accuracy("^7You: ^340%"), Some(("you", 40.0)));
        assert_eq!(accuracy("^7You: ^2hello"), None);
    }

    #[test]
    fn session_times() {
        use chrono::{Local, TimeZone};
        let t = session_start("logfile opened on Sun Nov 22 13:39:53 2026").unwrap();
        assert_eq!(t, Local.with_ymd_and_hms(2026, 11, 22, 13, 39, 53).unwrap().timestamp() as f64);
        assert!(session_start("logfile opened on Mon Nov  2 09:05:03 2026").is_some());
        assert!(session_start("something else").is_none());
    }

    #[test]
    fn switch_lines() {
        let before = "exec x76.cfg\r\nseta name \"x\"\r\n";
        let on = add(before);
        assert!(is_on(&on) && has_ours(&on));
        assert!(on.starts_with(before) && on.ends_with("set logfile 2\r\n"));
        assert_eq!(add(&on), on);
        assert_eq!(strip(&on), before);
        assert!(!is_on(before));
        assert!(is_on("seta logfile \"1\""));
        assert!(!is_on("// set logfile 2"));
        assert_eq!(add(""), format!("{MARK}\r\n{SET_LINE}\r\n"));
    }

    /// Reads this PC's real console log: `cargo test qllog -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_log() {
        let home = find_home(None).expect("no Quake Live account folder");
        println!("home {} · account {:?} · name {:?} · switch {}", home.display(), account_of(&home), my_name(&home), state(&home));
        let mut tail = Tail { home: Some(home), last_find: f64::MAX, ..Default::default() };
        let mut got = scan(&mut tail, None, &[]);
        got.extend(tail.pending.take().map(|p| p.0));
        for f in &got {
            let when = chrono::DateTime::from_timestamp(f.t as i64, 0).map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string());
            println!("{when:?} {} {} · me {} acc {:?} · them {:?} · {:?}", f.kind, f.opponent, f.me, f.my_acc, f.opp_acc, f.src);
        }
    }

    #[test]
    fn reads_fights_with_their_accuracy() {
        let dir = std::env::temp_dir().join(format!("x76-qllog-{}", std::process::id())).join("76561198119029051").join("baseq3");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("qzconfig.cfg"), "seta name \"^5something has to change\"\r\n").unwrap();
        let log = "logfile opened on Sun Nov 22 13:39:53 2026\n\n\
            ^3chamame^7^7 was electrocuted by ^5am9u^7^7\n\
            ^5something has to change^7^7 was electrocuted by ^5am9u^7^7\n\
            ^7Enemy: ^147\u{ff05}\n^7You: ^340\u{ff05}\n\
            ^5am9u^7^7 was electrocuted by ^5something has to change^7^7\n\
            ^7Enemy: ^147\u{ff05}\n";
        std::fs::write(dir.join(LOG), log).unwrap();
        let mut tail = Tail { home: Some(dir.clone()), last_find: f64::MAX, ..Default::default() };
        let got = scan(&mut tail, None, &[]);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].kind.as_str(), got[0].opponent.as_str(), got[0].me.as_str()), ("death", "am9u", "76561198119029051"));
        assert_eq!((got[0].my_acc, got[0].opp_acc), (Some(40.0), Some(47.0)));
        // the kill waits for its "You:" line, which arrives later
        assert!(tail.pending.is_some());
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join(LOG)).unwrap();
        std::io::Write::write_all(&mut f, "^7You: ^349\u{ff05}\n".as_bytes()).unwrap();
        let got2 = scan(&mut tail, None, &got);
        assert_eq!(got2.len(), 1);
        assert_eq!((got2[0].kind.as_str(), got2[0].my_acc, got2[0].opp_acc), ("kill", Some(49.0), Some(47.0)));
        // reading the same log again (a restart) finds nothing new
        let known: Vec<Fight> = got.into_iter().chain(got2).collect();
        let mut fresh = Tail { home: Some(dir.clone()), last_find: f64::MAX, ..Default::default() };
        assert!(scan(&mut fresh, None, &known).is_empty());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }
}
