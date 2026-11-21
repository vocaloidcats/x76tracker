//! x76 Elo Tracker, native edition. A window drawn with Slint; a background thread checks the
//! x76 ladders and records every change for the players you track.
//!
//!   x76EloTracker.exe                         normal start
//!   x76EloTracker.exe --data-dir D:\x76       keep history somewhere else
//!   x76EloTracker.exe --screenshot out.png [--size 1440x1000] [--light|--dark] [--hover x,y] [--offline]
//!                                             render the window to a PNG without showing it
#![windows_subsystem = "windows"]

slint::include_modules!();

mod tracker;
mod view;

use serde_json::Value;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracker::{now, Game, Hit, Shared, APP_ID, INTERVALS, VERSION};
use view::{Period, UiState};

thread_local! {
    // UI-thread-only state (expanded lists, pending remove, search box state)
    static UI: RefCell<UiState> = RefCell::new(UiState::default());
}

const SEARCH_SHOWN: usize = 8;

fn gi(g: Game) -> usize {
    match g {
        Game::Ql => 0,
        Game::R5 => 1,
    }
}

fn period_of(core: &tracker::Core) -> Period {
    Period::from_key(&core.setting_str("period").unwrap_or_default())
}

// ------------------------------------------------------------------ rendering
/// Rebuilds both game columns from the tracker state. Runs after checks and user actions.
fn refresh(app: &AppWindow, shared: &Shared) {
    let mut core = shared.lock();
    let fresh = std::mem::take(&mut core.fresh);
    let period = period_of(&core);
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        ui.flash = fresh;
        app.set_ql(view::game_data(&core, Game::Ql, period, &ui));
        app.set_r5(view::game_data(&core, Game::R5, period, &ui));
        ui.flash.clear();
    });
    app.set_period(period.index());
    app.set_interval_idx(INTERVALS.iter().position(|&i| i == core.interval()).unwrap_or(1) as i32);
    drop(core);
    tick(app, shared);
}

/// Cheap per-second update of the "live · 12s ago" and per-game status lines.
fn tick(app: &AppWindow, shared: &Shared) {
    // keep the maximize/restore icon right after Win+Up, snapping, or a double-click
    app.global::<WindowCtl>().set_maximized(app.window().is_maximized());
    let core = shared.lock();
    let (live, state) = view::live_status(&core);
    app.set_live_text(live.into());
    app.set_live_state(state);
    let (t, e) = view::game_status(&core, Game::Ql);
    app.set_ql_status(t.into());
    app.set_ql_status_error(e);
    let (t, e) = view::game_status(&core, Game::R5);
    app.set_r5_status(t.into());
    app.set_r5_status_error(e);
}

fn set_results(app: &AppWindow, game: Game, hits: &[Hit], msg: &str) {
    match game {
        Game::Ql => {
            app.set_ql_results(view::results(hits));
            app.set_ql_search_msg(msg.into());
        }
        Game::R5 => {
            app.set_r5_results(view::results(hits));
            app.set_r5_search_msg(msg.into());
        }
    }
}

fn search_message(game: Game, query: &str, shown: usize, total: usize) -> String {
    if total == 0 {
        let hint = if game == Game::R5 { " R5 players are matched by in-game name." } else { "" };
        format!("No player matching “{}”.{hint}", query.trim())
    } else if total > shown {
        format!("Showing {shown} of {total} matches. Keep typing to narrow it down.")
    } else {
        String::new()
    }
}

fn tracked_ids(shared: &Shared, game: Game) -> HashSet<String> {
    shared.lock().state.favorites.iter().filter(|f| f.game == game.id()).map(|f| f.id.clone()).collect()
}

fn on_search(app: &AppWindow, shared: &Arc<Shared>, game: Game, query: String) {
    let seq = UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        ui.query[gi(game)] = query.clone();
        ui.search_seq[gi(game)] += 1;
        ui.search_seq[gi(game)]
    });
    if query.trim().is_empty() {
        set_results(app, game, &[], "");
        return;
    }
    let tracked = tracked_ids(shared, game);
    if let Some(board) = shared.cached_board(game) {
        let (hits, total) = tracker::search(&board, game, &query, &tracked, SEARCH_SHOWN);
        set_results(app, game, &hits, &search_message(game, &query, hits.len(), total));
        return;
    }
    set_results(app, game, &[], &format!("Loading the {} ladder…", game.label()));
    let (weak, shared) = (app.as_weak(), shared.clone());
    std::thread::spawn(move || {
        let outcome = shared.search_board(game).map(|b| tracker::search(&b, game, &query, &tracked, SEARCH_SHOWN));
        let _ = weak.upgrade_in_event_loop(move |app| {
            if UI.with(|ui| ui.borrow().search_seq[gi(game)]) != seq {
                return; // the user kept typing; a newer search is on its way
            }
            match outcome {
                Ok((hits, total)) => set_results(&app, game, &hits, &search_message(game, &query, hits.len(), total)),
                Err(e) => set_results(&app, game, &[], &format!("Couldn't load the ladder: {e}")),
            }
            tick(&app, &shared);
        });
    });
}

/// Re-run the visible search so "Track" buttons show the right state.
fn rerun_search(app: &AppWindow, shared: &Arc<Shared>, game: Game) {
    let q = UI.with(|ui| ui.borrow().query[gi(game)].clone());
    if !q.trim().is_empty() {
        on_search(app, shared, game, q);
    }
}

// ------------------------------------------------------------------ wiring
fn wire(app: &AppWindow, shared: &Arc<Shared>) {
    app.global::<ChartMath>().on_step_path(|pts, w, h, area| view::step_path(&pts, w, h, area));
    let actions = app.global::<Actions>();

    // Each handler gets its own handle to the window and the tracker.
    macro_rules! ctx {
        () => {
            (app.as_weak(), shared.clone())
        };
    }

    actions.on_period({
        let (w, s) = ctx!();
        move |i| {
            s.lock().set_setting("period", Period::from_index(i).key().into());
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_interval({
        let (w, s) = ctx!();
        move |i| {
            let secs = INTERVALS.get(i as usize).copied().unwrap_or(INTERVALS[1]);
            s.lock().set_setting("interval", secs.into());
            s.wake();
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_refresh({
        let (_, s) = ctx!();
        move || s.wake()
    });
    actions.on_theme({
        let (_, s) = ctx!();
        move |dark| s.lock().set_setting("theme", (if dark { "dark" } else { "light" }).into())
    });
    actions.on_search({
        let (w, s) = ctx!();
        move |game, query| {
            if let (Some(app), Some(g)) = (w.upgrade(), Game::from_id(&game)) {
                on_search(&app, &s, g, query.to_string());
            }
        }
    });
    actions.on_track({
        let (w, s) = ctx!();
        move |game, id| {
            let Some(g) = Game::from_id(&game) else { return };
            let (w, s) = (w.clone(), s.clone());
            std::thread::spawn(move || {
                let result = s.add_favorite(g, &id);
                if result.is_ok() && s.lock().next_poll.map_or(true, |t| t < now()) {
                    s.wake(); // first player: start the check loop now
                }
                let _ = w.upgrade_in_event_loop(move |app| {
                    if let Err(e) = result {
                        app.set_banner(format!("Couldn't track that player: {e}").into());
                    }
                    refresh(&app, &s);
                    rerun_search(&app, &s, g);
                });
            });
        }
    });
    actions.on_remove_ask({
        let (w, s) = ctx!();
        move |key| {
            UI.with(|ui| ui.borrow_mut().confirm = Some(key.to_string()));
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_remove_cancel({
        let (w, s) = ctx!();
        move || {
            UI.with(|ui| ui.borrow_mut().confirm = None);
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_remove({
        let (w, s) = ctx!();
        move |game, id| {
            s.lock().remove_favorite(&game, &id);
            UI.with(|ui| ui.borrow_mut().confirm = None);
            if let (Some(app), Some(g)) = (w.upgrade(), Game::from_id(&game)) {
                refresh(&app, &s);
                rerun_search(&app, &s, g);
            }
        }
    });
    actions.on_move({
        let (w, s) = ctx!();
        move |game, id, dir| {
            s.lock().move_favorite(&game, &id, dir);
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_metric({
        let (w, s) = ctx!();
        move |key, elo| {
            {
                let mut core = s.lock();
                let mut m = core.state.settings.get("metrics").and_then(Value::as_object).cloned().unwrap_or_default();
                if elo {
                    m.insert(key.to_string(), "elo".into());
                } else {
                    m.remove(key.as_str());
                }
                core.set_setting("metrics", Value::Object(m));
            }
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_show_all({
        let (w, s) = ctx!();
        move |key| {
            UI.with(|ui| {
                let mut ui = ui.borrow_mut();
                if !ui.show_all.remove(key.as_str()) {
                    ui.show_all.insert(key.to_string());
                }
            });
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_open_profile(|id| {
        if id.chars().all(|c| c.is_ascii_digit()) {
            let _ = std::process::Command::new("explorer").arg(format!("https://steamcommunity.com/profiles/{id}")).spawn();
        }
    });
    actions.on_dismiss_banner({
        let (w, _) = ctx!();
        move || {
            if let Some(app) = w.upgrade() {
                app.set_banner("".into());
            }
        }
    });
}

/// Custom title bar: native drag (with Windows snapping), minimize/maximize/close, edge resizing.
fn wire_window(app: &AppWindow) {
    use slint::winit_030::{winit::window::ResizeDirection, WinitWindowAccessor};
    let ctl = app.global::<WindowCtl>();
    let w = app.as_weak();
    ctl.on_drag({
        let w = w.clone();
        move || {
            if let Some(app) = w.upgrade() {
                app.window().with_winit_window(|win| {
                    let _ = win.drag_window();
                });
            }
        }
    });
    ctl.on_minimize({
        let w = w.clone();
        move || {
            if let Some(app) = w.upgrade() {
                app.window().set_minimized(true);
            }
        }
    });
    ctl.on_toggle_maximize({
        let w = w.clone();
        move || {
            if let Some(app) = w.upgrade() {
                let maximize = !app.window().is_maximized();
                app.window().set_maximized(maximize);
                app.global::<WindowCtl>().set_maximized(maximize);
            }
        }
    });
    ctl.on_close({
        let w = w.clone();
        move || {
            if let Some(app) = w.upgrade() {
                let _ = app.window().hide();
            }
        }
    });
    ctl.on_resize(move |direction| {
        let Some(app) = w.upgrade() else { return };
        let dir = match direction {
            0 => ResizeDirection::North,
            1 => ResizeDirection::NorthEast,
            2 => ResizeDirection::East,
            3 => ResizeDirection::SouthEast,
            4 => ResizeDirection::South,
            5 => ResizeDirection::SouthWest,
            6 => ResizeDirection::West,
            _ => ResizeDirection::NorthWest,
        };
        app.window().with_winit_window(|win| {
            let _ = win.drag_resize_window(dir);
        });
    });
}

/// One-time window setup: theme, footer info, any warning from loading history.
fn startup(app: &AppWindow, shared: &Shared) {
    let core = shared.lock();
    app.set_data_file(core.data_file.display().to_string().into());
    app.set_version(VERSION.into());
    if let Some(theme) = core.setting_str("theme") {
        app.invoke_set_dark(theme == "dark");
    }
    if let Some(w) = &core.load_warning {
        app.set_banner(w.as_str().into());
    }
}

// ------------------------------------------------------------------ guards
fn default_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("AppData").join("Local")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("x76EloTracker")
}

fn same_path(a: &str, b: &Path) -> bool {
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(&b.display().to_string())
}

/// Is the browser (Python) version answering on `port` and saving to the same history file?
fn browser_version_at(port: u16, data_file: &Path) -> bool {
    let agent = tracker::local_agent();
    let base = format!("http://127.0.0.1:{port}");
    let is_tracker = agent
        .get(format!("{base}/api/ping"))
        .call()
        .ok()
        .and_then(|mut r| r.body_mut().read_json::<Value>().ok())
        .is_some_and(|v| v.get("app").and_then(Value::as_str) == Some(APP_ID));
    is_tracker
        && agent
            .get(format!("{base}/api/state"))
            .call()
            .ok()
            .and_then(|mut r| r.body_mut().with_config().limit(256 * 1024 * 1024).read_json::<Value>().ok())
            .and_then(|v| v.pointer("/app/dataFile").and_then(Value::as_str).map(|p| same_path(p, data_file)))
            .unwrap_or(false)
}

/// Port of the browser version if it's running on the same history file. All the ports it may
/// use are probed at once: on Windows a refused local connection can take about a second.
fn browser_version_on(data_file: &Path) -> Option<u16> {
    let probes: Vec<_> = (7676..7686)
        .map(|port| {
            let file = data_file.to_path_buf();
            std::thread::spawn(move || browser_version_at(port, &file).then_some(port))
        })
        .collect();
    probes.into_iter().filter_map(|p| p.join().ok().flatten()).min()
}

enum Lock {
    Held(#[allow(dead_code)] std::fs::File),
    Busy,
    Unavailable,
}

/// One copy per history folder: a second launch just brings the first window forward.
fn single_instance(dir: &Path) -> Lock {
    if std::fs::create_dir_all(dir).is_err() {
        return Lock::Unavailable;
    }
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("app.lock")) else {
        return Lock::Unavailable;
    };
    match file.try_lock() {
        Ok(()) => Lock::Held(file),
        Err(std::fs::TryLockError::WouldBlock) => Lock::Busy,
        Err(_) => Lock::Unavailable,
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    pub const TITLE: &str = "Elo Tracker for x76";

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
        fn FindWindowW(class: *const u16, title: *const u16) -> *mut c_void;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    pub fn message(text: &str) {
        unsafe {
            MessageBoxW(null_mut(), wide(text).as_ptr(), wide(TITLE).as_ptr(), 0x40); // MB_ICONINFORMATION
        }
    }

    pub fn focus_existing() {
        unsafe {
            let h = FindWindowW(null(), wide(TITLE).as_ptr());
            if !h.is_null() {
                ShowWindow(h, 9); // SW_RESTORE
                SetForegroundWindow(h);
            }
        }
    }
}

#[cfg(not(windows))]
mod win {
    pub fn message(text: &str) {
        eprintln!("{text}");
    }
    pub fn focus_existing() {}
}

// ------------------------------------------------------------------ screenshot mode
mod screenshot {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter, WindowEvent};
    use std::rc::Rc;

    struct Offscreen(Rc<MinimalSoftwareWindow>);
    impl Platform for Offscreen {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    pub struct Options {
        pub out: String,
        pub size: (u32, u32),
        pub theme: Option<bool>,
        pub hover: Option<(f32, f32)>,
        pub offline: bool,
    }

    pub fn run(data_file: PathBuf, o: Options) -> Result<(), Box<dyn std::error::Error>> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Offscreen(window.clone()))).map_err(|e| format!("{e:?}"))?;
        let shared = Shared::new(data_file);
        if !o.offline {
            shared.poll_once(&|_| {});
        }
        let app = AppWindow::new()?;
        wire(&app, &shared);
        startup(&app, &shared);
        app.invoke_set_dark(o.theme.unwrap_or(true));
        refresh(&app, &shared);
        let (w, h) = o.size;
        window.set_size(slint::PhysicalSize::new(w, h));
        app.show()?;
        let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        for pass in 0..3 {
            if pass == 2 {
                if let Some((x, y)) = o.hover {
                    app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(x, y) });
                }
            }
            window.request_redraw();
            window.draw_if_needed(|r| {
                r.render(&mut buf, w as usize);
            });
        }
        let mut rgba = Vec::with_capacity(buf.len() * 4);
        for p in &buf {
            let a = p.alpha as u32;
            let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
            rgba.extend_from_slice(&[un(p.red), un(p.green), un(p.blue), p.alpha]);
        }
        let file = std::io::BufWriter::new(std::fs::File::create(&o.out)?);
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&rgba)?;
        Ok(())
    }
}

// ------------------------------------------------------------------ main
fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn pair<T: std::str::FromStr>(v: &str, sep: char) -> Option<(T, T)> {
    let (a, b) = v.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = arg_value(&args, "--data-dir").map(PathBuf::from).unwrap_or_else(default_data_dir);
    let data_file = data_dir.join("state.json");

    if let Some(out) = arg_value(&args, "--screenshot") {
        let theme = if args.iter().any(|a| a == "--light") { Some(false) } else if args.iter().any(|a| a == "--dark") { Some(true) } else { None };
        return screenshot::run(
            data_file,
            screenshot::Options {
                out,
                size: arg_value(&args, "--size").and_then(|v| pair(&v, 'x')).unwrap_or((1440, 1000)),
                theme,
                hover: arg_value(&args, "--hover").and_then(|v| pair(&v, ',')),
                offline: args.iter().any(|a| a == "--offline"),
            },
        );
    }

    // single-copy check first: it's instant, so a second launch hands off right away
    let _lock = match single_instance(&data_dir) {
        Lock::Busy => {
            win::focus_existing();
            return Ok(());
        }
        lock => lock,
    };
    if let Some(port) = browser_version_on(&data_file) {
        win::message(&format!(
            "The browser version of Elo Tracker is running (port {port}) and saves to the same history.\n\n\
             Close its black console window first, then start this app again."
        ));
        return Ok(());
    }

    // Software renderer: draws text cleanly (the GPU renderer nudged some letters off the line).
    // The window has no Windows title bar; keep Windows 11's rounded corners and shadow anyway.
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .with_winit_window_attributes_hook(|attrs| {
            #[cfg(windows)]
            let attrs = {
                use slint::winit_030::winit::platform::windows::{CornerPreference, WindowAttributesExtWindows};
                attrs.with_undecorated_shadow(true).with_corner_preference(CornerPreference::Round)
            };
            attrs
        })
        .select()?;

    let shared = Shared::new(data_file);
    let app = AppWindow::new()?;
    wire(&app, &shared);
    wire_window(&app);
    startup(&app, &shared);
    refresh(&app, &shared);

    let (poller, notify_to, weak) = (shared.clone(), shared.clone(), app.as_weak());
    std::thread::spawn(move || {
        poller.run(move |full| {
            let s = notify_to.clone();
            let _ = weak.upgrade_in_event_loop(move |app| if full { refresh(&app, &s) } else { tick(&app, &s) });
        })
    });

    let ticker = slint::Timer::default();
    let (w, s) = (app.as_weak(), shared.clone());
    let mut seconds = 0u32;
    ticker.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        seconds += 1;
        if let Some(app) = w.upgrade() {
            // a full refresh each minute keeps "today" windows and relative times right
            if seconds % 60 == 0 { refresh(&app, &s) } else { tick(&app, &s) }
        }
    });

    app.run()?;
    Ok(())
}

fn main() {
    if let Err(e) = run(std::env::args().collect()) {
        win::message(&format!("Elo Tracker hit an error and stopped:\n\n{e}"));
    }
}
