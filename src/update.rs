//! Updates: asks GitHub for the latest release, downloads it, checks it against the SHA-256
//! published with the release, and swaps it in for the running exe (on close, or on "Install and
//! restart"). Also the in-app changelog, which is CHANGELOG.md built into the exe.

use crate::tracker::{now, Shared, VERSION};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub const REPO: &str = "vocaloidcats/x76tracker";
pub const RELEASES_PAGE: &str = "https://github.com/vocaloidcats/x76tracker/releases";
const EXE_ASSET: &str = "x76tracker.exe";
const SUM_ASSET: &str = "x76tracker.exe.sha256";
/// How often the background check asks GitHub (its API allows 60 requests an hour without login).
pub const CHECK_EVERY: f64 = 6.0 * 3600.0;
const MAX_DOWNLOAD: u64 = 200 * 1024 * 1024;

pub const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// "Install and restart" was pressed: swap the new version in on exit, then start it.
pub static RESTART: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Default)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub page: String,
    pub exe_url: Option<String>,
    pub sum_url: Option<String>,
}

/// Update state that isn't saved.
#[derive(Default)]
pub struct Live {
    pub checked_at: Option<f64>,
    pub latest: Option<Release>,
    pub error: Option<String>,
    pub checking: bool,
    /// Download progress, 0..1, while downloading.
    pub progress: Option<f32>,
    /// A downloaded and verified new version: (version, file).
    pub staged: Option<(String, PathBuf)>,
    /// The last download that failed: (version, when). Background downloads wait before retrying.
    pub failed: Option<(String, f64)>,
}

impl Live {
    /// Whether the background may download the newer version now (not right after a failure).
    pub fn auto_download_allowed(&self) -> bool {
        match (self.newer(), &self.failed) {
            (Some(r), Some((v, t))) => !(r.version == *v && now() - t < CHECK_EVERY),
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// The latest release, if it's newer than this version.
    pub fn newer(&self) -> Option<&Release> {
        self.latest.as_ref().filter(|r| is_newer(&r.version, VERSION))
    }
}

// ------------------------------------------------------------------ versions
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next().flatten().unwrap_or(0), parts.next().flatten().unwrap_or(0)))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

/// Screenshot previews (`--preview-update`) show the update screens as an installed release sees them.
pub static PREVIEW: AtomicBool = AtomicBool::new(false);

/// A pretend release for screenshot previews: its notes are that version's CHANGELOG.md section,
/// or the newest section when the version has none yet.
pub fn preview_release(version: &str) -> Release {
    let version = version.trim().trim_start_matches(['v', 'V']).to_string();
    let notes = section_text(CHANGELOG, &version)
        .or_else(|| parse_changelog(CHANGELOG, "").first().and_then(|e| section_text(CHANGELOG, &e.version)))
        .unwrap_or_default();
    Release { version, notes, page: RELEASES_PAGE.to_string(), exe_url: None, sum_url: None }
}

/// Run from cargo's target folder: never replace it with a release.
pub fn dev_build() -> bool {
    if PREVIEW.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    std::env::current_exe()
        .map(|p| {
            let p = p.to_string_lossy().to_lowercase().replace('/', "\\");
            p.contains("\\target\\release\\") || p.contains("\\target\\debug\\")
        })
        .unwrap_or(false)
}

// ------------------------------------------------------------------ settings
pub fn auto_check(core: &crate::tracker::Core) -> bool {
    core.state.settings.get("updateCheck").and_then(Value::as_bool).unwrap_or(true)
}

pub fn auto_install(core: &crate::tracker::Core) -> bool {
    core.state.settings.get("updateAuto").and_then(Value::as_bool).unwrap_or(true)
}

// ------------------------------------------------------------------ GitHub
pub fn fetch_latest() -> Result<Release, String> {
    let v: Value = crate::tracker::agent(Duration::from_secs(20))
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("Couldn't reach GitHub: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("GitHub sent something unexpected: {e}"))?;
    let tag = v.get("tag_name").and_then(Value::as_str).ok_or("GitHub's answer had no release in it.")?;
    let asset = |name: &str| {
        v.get("assets")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find(|x| x.get("name").and_then(Value::as_str) == Some(name)))
            .and_then(|x| x.get("browser_download_url").and_then(Value::as_str))
            .map(str::to_string)
    };
    Ok(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        notes: v.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        page: v.get("html_url").and_then(Value::as_str).unwrap_or(RELEASES_PAGE).to_string(),
        exe_url: asset(EXE_ASSET),
        sum_url: asset(SUM_ASSET),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash in a `.sha256` file ("<64 hex>  x76tracker.exe").
pub fn parse_sum(text: &str) -> Option<String> {
    let h = text.trim_start_matches('\u{feff}').split_whitespace().next()?.to_lowercase();
    (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

/// Downloads the release's exe into `dir` and checks its SHA-256. Returns the verified file.
pub fn download(rel: &Release, dir: &Path, progress: &dyn Fn(f32)) -> Result<PathBuf, String> {
    let (Some(exe_url), Some(sum_url)) = (&rel.exe_url, &rel.sum_url) else {
        return Err("This release has no checksum file, so it can't be installed automatically. Download it from the release page.".into());
    };
    let agent = crate::tracker::agent(Duration::from_secs(600));
    let sum_text = agent
        .get(sum_url)
        .call()
        .map_err(|e| format!("Couldn't download the checksum: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let want = parse_sum(&sum_text).ok_or("The release's checksum file isn't readable.")?;

    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let part = dir.join(format!("x76tracker-{}.exe.part", rel.version));
    let mut resp = agent.get(exe_url).call().map_err(|e| format!("Couldn't download the update: {e}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = resp.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let mut out = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let (mut buf, mut got) = (vec![0u8; 64 * 1024], 0u64);
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("The download broke off: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        if let Some(t) = total.filter(|t| *t > 0) {
            progress((got as f64 / t as f64).min(1.0) as f32);
        }
    }
    drop(out);
    if hex(&hasher.finalize()) != want {
        let _ = std::fs::remove_file(&part);
        return Err("The download didn't match the SHA-256 published with the release, so it wasn't installed.".into());
    }
    let done = dir.join(format!("x76tracker-{}.exe", rel.version));
    let _ = std::fs::remove_file(&done);
    std::fs::rename(&part, &done).map_err(|e| e.to_string())?;
    Ok(done)
}

// ------------------------------------------------------------------ swapping the exe
fn old_path(exe: &Path) -> PathBuf {
    let stem = exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "x76tracker".into());
    exe.with_file_name(format!("{stem}.old.exe"))
}

/// Puts `staged` where `exe` is. Windows lets a running exe be renamed (not deleted), so the
/// running one moves aside to `<name>.old.exe` and is removed on the next start.
pub fn swap_at(exe: &Path, staged: &Path) -> Result<(), String> {
    let old = old_path(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| {
        format!("Couldn't move the running version aside ({e}). If x76tracker is in a protected folder like Program Files, move it somewhere you can write to, or download the update from the release page.")
    })?;
    if let Err(e) = std::fs::copy(staged, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("Couldn't put the new version in place: {e}"));
    }
    let _ = std::fs::remove_file(staged);
    Ok(())
}

/// Removes what the last update left behind (the old exe, stale downloads).
pub fn clean_up(update_dir: &Path) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
    if let Ok(entries) = std::fs::read_dir(update_dir) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// ------------------------------------------------------------------ the background work
/// Asks GitHub for the latest release. Returns true when done (the UI shows the outcome).
pub fn check(shared: &Shared) -> bool {
    {
        let mut core = shared.lock();
        if core.updater.checking || core.updater.progress.is_some() {
            return false;
        }
        core.updater.checking = true;
    }
    let result = fetch_latest();
    let mut core = shared.lock();
    core.updater.checking = false;
    core.updater.checked_at = Some(now());
    match result {
        Ok(r) => {
            core.updater.latest = Some(r);
            core.updater.error = None;
        }
        Err(e) => core.updater.error = Some(e),
    }
    true
}

/// Downloads the newer release if there is one and it isn't downloaded yet. Returns whether a
/// verified copy is ready to swap in.
pub fn fetch_update(shared: &Shared, notify: &dyn Fn()) -> bool {
    let (rel, dir) = {
        let mut core = shared.lock();
        let Some(rel) = core.updater.newer().cloned() else { return false };
        if core.updater.staged.as_ref().is_some_and(|(v, p)| *v == rel.version && p.is_file()) {
            return true;
        }
        if core.updater.progress.is_some() {
            return false; // already downloading
        }
        core.updater.progress = Some(0.0);
        core.updater.error = None;
        (rel, core.data_file.with_file_name("update"))
    };
    notify();
    let last = std::sync::Mutex::new(0.0f32);
    let result = download(&rel, &dir, &|p| {
        shared.lock().updater.progress = Some(p);
        let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
        if p - *l >= 0.05 {
            *l = p;
            notify();
        }
    });
    let mut core = shared.lock();
    core.updater.progress = None;
    let ok = result.is_ok();
    match result {
        Ok(path) => core.updater.staged = Some((rel.version.clone(), path)),
        Err(e) => {
            core.updater.failed = Some((rel.version.clone(), now()));
            core.updater.error = Some(e);
        }
    }
    drop(core);
    notify();
    ok
}

// ------------------------------------------------------------------ changelog
/// A picture in the changelog: a repo path (docs/...) or a full URL.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Media {
    pub src: String,
    pub alt: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    /// bold lead-in ("Share card"), may be empty
    pub lead: String,
    pub text: String,
    pub media: Option<Media>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Section {
    /// "New", "Improved", "Fixed"… ("" for items before any ### heading)
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub version: String,
    pub note: String,
    /// a picture before the first item: shown big at the top
    pub hero: Option<Media>,
    pub sections: Vec<Section>,
}

impl Entry {
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.sections.iter().flat_map(|s| s.items.iter())
    }
    pub fn media(&self) -> impl Iterator<Item = &Media> {
        self.hero.iter().chain(self.items().filter_map(|i| i.media.as_ref()))
    }
}

fn plain(s: &str) -> String {
    s.replace("**", "").replace('`', "").trim().to_string()
}

/// "![alt](src)" → Media
fn picture(line: &str) -> Option<Media> {
    let rest = line.strip_prefix("![")?;
    let (alt, rest) = rest.split_once("](")?;
    let src = rest.strip_suffix(')')?.split_whitespace().next()?.to_string();
    Some(Media { src, alt: alt.to_string() })
}

/// The raw text of one version's section (without its "## " heading).
pub fn section_text(md: &str, version: &str) -> Option<String> {
    let mut lines = md.lines().skip_while(|l| l.trim().strip_prefix("## ").map(|v| v.trim().trim_start_matches(['v', 'V'])) != Some(version));
    lines.next()?;
    let body: Vec<&str> = lines.take_while(|l| !l.trim_start().starts_with("## ")).collect();
    Some(body.join("\n").trim().to_string())
}

/// Reads CHANGELOG.md-style text: "## 2.3.0" versions, "### New" sections, "- **Lead:** text"
/// items, "![alt](path)" pictures (before the first item: the version's hero; otherwise the
/// item above's), other lines as notes. Items before any heading go under `untitled` (a
/// release's notes have no heading).
pub fn parse_changelog(md: &str, untitled: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for raw in md.lines() {
        let line = raw.trim();
        if let Some(v) = line.strip_prefix("## ") {
            out.push(Entry { version: v.trim().trim_start_matches(['v', 'V']).to_string(), ..Entry::default() });
            continue;
        }
        if line.is_empty() || line.starts_with("# ") {
            continue;
        }
        if out.is_empty() {
            if untitled.is_empty() {
                continue; // the file's own introduction
            }
            out.push(Entry { version: untitled.to_string(), ..Entry::default() });
        }
        let e = out.last_mut().unwrap();
        if let Some(title) = line.strip_prefix("### ") {
            e.sections.push(Section { title: plain(title), items: Vec::new() });
            continue;
        }
        if let Some(m) = picture(line) {
            let last = e.sections.iter_mut().rev().find_map(|s| s.items.last_mut());
            match last {
                Some(item) if item.media.is_none() => item.media = Some(m),
                Some(_) => {}
                None if e.hero.is_none() => e.hero = Some(m),
                None => {}
            }
            continue;
        }
        if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            let item = item.trim();
            let split = item.strip_prefix("**").and_then(|rest| rest.split_once(":**").map(|(lead, text)| (lead.to_string(), text.to_string())));
            // on its own line under the bold lead-in, the text starts a sentence (names like
            // "x76tracker" keep their spelling)
            let capital = |t: String| {
                if !t.split_whitespace().next().is_some_and(|w| w.chars().all(char::is_alphabetic)) {
                    return t;
                }
                let mut c = t.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            };
            let (lead, text) = match split {
                Some((lead, text)) => (plain(&lead), capital(plain(&text))),
                None => (String::new(), plain(item)),
            };
            if e.sections.is_empty() {
                e.sections.push(Section::default());
            }
            e.sections.last_mut().unwrap().items.push(Item { lead, text, media: None });
        } else if e.sections.iter().all(|s| s.items.is_empty()) {
            if !e.note.is_empty() {
                e.note.push(' ');
            }
            e.note += &plain(line);
        }
    }
    out
}

// ------------------------------------------------------------------ changelog pictures
const RAW: &str = "https://raw.githubusercontent.com/vocaloidcats/x76tracker/main/";
const MAX_MEDIA: u64 = 25 * 1024 * 1024;
const MAX_FRAMES: usize = 180;

fn cache_name(src: &str) -> String {
    src.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

/// A changelog picture's bytes: from the repository when this is a cargo build inside it, else
/// from the cache, else from GitHub (then cached).
pub fn media_bytes(src: &str, cache_dir: &Path) -> Result<Vec<u8>, String> {
    let url = if src.starts_with("https://") {
        src.to_string()
    } else {
        let rel = src.trim_start_matches("./");
        if let Some(repo) = std::env::current_exe().ok().and_then(|e| e.parent()?.parent()?.parent().map(Path::to_path_buf)) {
            let local = repo.join(rel);
            if dev_build() && local.is_file() {
                return std::fs::read(&local).map_err(|e| e.to_string());
            }
        }
        format!("{RAW}{rel}")
    };
    let cached = cache_dir.join(cache_name(src));
    if let Ok(b) = std::fs::read(&cached) {
        return Ok(b);
    }
    let bytes = crate::tracker::agent(Duration::from_secs(60))
        .get(&url)
        .call()
        .map_err(|e| format!("Couldn't download {src}: {e}"))?
        .body_mut()
        .with_config()
        .limit(MAX_MEDIA)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::create_dir_all(cache_dir);
    let _ = std::fs::write(&cached, &bytes);
    Ok(bytes)
}

/// Decoded picture: its distinct RGBA frames (one for a still), and the order to show them in at
/// `frame_ms` each (a frame held longer appears several times in a row).
pub struct Frames {
    pub width: u32,
    pub height: u32,
    pub frame_ms: u32,
    pub frames: Vec<Vec<u8>>,
    pub order: Vec<usize>,
}

/// Decodes a PNG or an animated PNG (APNG); anything else the image library reads works as a still.
pub fn decode_media(bytes: &[u8]) -> Result<Frames, String> {
    use image::AnimationDecoder;
    if let Ok(dec) = image::codecs::png::PngDecoder::new(std::io::Cursor::new(bytes)) {
        if dec.is_apng().unwrap_or(false) {
            let frames = dec
                .apng()
                .map_err(|e| e.to_string())?
                .into_frames()
                .take(MAX_FRAMES)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            let first = frames.first().ok_or("an animation with no frames")?;
            let (width, height) = first.buffer().dimensions();
            let ms = |f: &image::Frame| {
                let (n, d) = f.delay().numer_denom_ms();
                if d == 0 { 50 } else { (n / d).max(10) }
            };
            // the shortest frame sets the pace; longer ones repeat
            let frame_ms = frames.iter().map(ms).min().unwrap_or(50);
            let mut order = Vec::new();
            for (i, f) in frames.iter().enumerate() {
                let times = ((ms(f) as f32 / frame_ms as f32).round() as usize).max(1);
                order.extend(std::iter::repeat(i).take(times));
            }
            return Ok(Frames { width, height, frame_ms, frames: frames.into_iter().map(|f| f.into_buffer().into_raw()).collect(), order });
        }
    }
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?.to_rgba8();
    let (width, height) = img.dimensions();
    Ok(Frames { width, height, frame_ms: 0, frames: vec![img.into_raw()], order: vec![0] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("2.3.0", "2.2.1"));
        assert!(is_newer("v2.10.0", "2.9.9"));
        assert!(!is_newer("2.2.1", "2.2.1"));
        assert!(!is_newer("2.2.0", "2.3.0"));
        assert!(!is_newer("nonsense", "2.3.0"));
        assert_eq!(parse_version("v3"), Some((3, 0, 0)));
    }

    #[test]
    fn checksum_file() {
        let h = "f27a22452a2593030f36d17147ceb618af7a3dc577b447c460897de431c9a4ed";
        assert_eq!(parse_sum(&format!("{h}  x76tracker.exe\r\n")).as_deref(), Some(h));
        assert_eq!(parse_sum(&format!("\u{feff}{}  x76tracker.exe", h.to_uppercase())).as_deref(), Some(h));
        assert_eq!(parse_sum("not a hash"), None);
        assert_eq!(hex(&Sha256::digest(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn the_built_in_changelog_reads() {
        let entries = parse_changelog(CHANGELOG, "");
        assert_eq!(entries.first().map(|e| e.version.as_str()), Some(VERSION), "CHANGELOG.md needs a section for this version");
        assert!(entries.iter().all(|e| e.items().next().is_some()));
        let first = entries[0].items().next().unwrap();
        assert!(!first.lead.contains('*') && !first.text.contains('*'));
        // every picture the changelog names is in the repository
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        for m in entries.iter().flat_map(|e| e.media()) {
            assert!(repo.join(&m.src).is_file(), "CHANGELOG.md shows {} but the file isn't there", m.src);
        }
    }

    /// The changelog's animations decode, and each wave comes to rest exactly where it started.
    #[test]
    fn changelog_animations_loop_cleanly() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs").join("changelog");
        for name in ["2.2.2-upset@2x.png", "2.2.2-supporter@2x.png"] {
            let f = decode_media(&std::fs::read(dir.join(name)).unwrap()).unwrap();
            assert!(f.frames.len() > 10, "{name} should be animated");
            assert_eq!(f.frames.first(), f.frames.last(), "{name} should end on the picture it started on");
            assert!(f.order.len() >= f.frames.len());
        }
    }

    #[test]
    fn changelog_structure() {
        let md = "## 9.9.9\n\nIntro line.\n\n![hero](docs/a.png)\n\n### New\n- **Thing:** does stuff\n  ![shot](docs/b.png)\n- plain item\n\n### Fixed\n- **Bug:** gone\n\n## 9.9.8\n- old";
        let e = parse_changelog(md, "");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].note, "Intro line.");
        assert_eq!(e[0].hero.as_ref().map(|m| m.src.as_str()), Some("docs/a.png"));
        assert_eq!(e[0].sections.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["New", "Fixed"]);
        let thing = &e[0].sections[0].items[0];
        assert_eq!((thing.lead.as_str(), thing.text.as_str()), ("Thing", "Does stuff"));
        assert_eq!(thing.media.as_ref().map(|m| m.alt.as_str()), Some("shot"));
        assert_eq!(e[0].sections[0].items[1].text, "plain item");
        assert_eq!(section_text(md, "9.9.8").as_deref(), Some("- old"));
        let notes = parse_changelog("- **New:** thing\n**Full Changelog**: https://x", "1.0.0");
        assert_eq!(notes[0].version, "1.0.0");
        assert_eq!(notes[0].items().next().map(|i| i.text.as_str()), Some("Thing"));
    }

    #[test]
    fn swapping_files() {
        let dir = std::env::temp_dir().join(format!("x76tracker-swap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (exe, staged) = (dir.join("x76tracker.exe"), dir.join("new.exe"));
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        swap_at(&exe, &staged).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.join("x76tracker.old.exe")).unwrap(), b"old");
        assert!(!staged.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Downloads the current GitHub release and checks it against its checksum (network).
    /// `cargo test --release update -- --ignored`
    #[test]
    #[ignore]
    fn real_release_downloads_and_verifies() {
        let rel = fetch_latest().unwrap();
        let dir = std::env::temp_dir().join("x76tracker-update-test");
        let path = download(&rel, &dir, &|_| {}).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 1_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
