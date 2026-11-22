//! x76tracker. A window drawn with Slint; a background thread checks the
//! x76 ladders and records every change for the players you track.
//!
//!   x76tracker.exe                            normal start
//!   x76tracker.exe --data-dir D:\x76          keep history somewhere else
//!   x76tracker.exe --screenshot out.png [--size 1440x1000] [--light|--dark] [--hover x,y] [--offline]
//!                  [--tab servers] [--settings general|widget]
//!                                             render the window to a PNG without showing it
#![windows_subsystem = "windows"]

slint::include_modules!();

mod discord;
mod profile;
mod r5log;
mod ranks;
mod rpc;
mod servers;
mod tracker;
mod view;

use serde_json::Value;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracker::{now, Game, Hit, Shared, INTERVALS, VERSION};
use view::{Period, UiState};

thread_local! {
    // UI-thread-only state (expanded lists, pending remove, search box state)
    static UI: RefCell<UiState> = RefCell::new(UiState::default());
    // whether profile pictures are loaded: (avatar, banner)
    static PICS: RefCell<(bool, bool)> = const { RefCell::new((false, false)) };
    // what the Discord settings page was last built from (see set_discord_page)
    static DISCORD_SIG: RefCell<String> = const { RefCell::new(String::new()) };
}

type Pixels = Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>;

/// Fetches the public x76 profile and its pictures (blocking; call off the UI thread).
fn fetch_profile(shared: &Shared, handle: &str) -> Result<(Pixels, Pixels), String> {
    let p = profile::fetch(handle)?;
    // both ladders, for the rank tiles (cached copies are reused if they're under 5 minutes old)
    if p.steam_id.is_some() {
        let _ = shared.board(Game::Ql, 300.0);
    }
    let _ = shared.board(Game::R5, 300.0);
    let avatar = p.avatar_url.as_deref().and_then(|u| profile::picture(u, 256, true));
    let banner = p.banner_url.as_deref().and_then(|u| profile::picture(u, 1600, false));
    let mut core = shared.lock();
    core.set_setting("x76Handle", p.handle.clone().into());
    core.profile = Some(p);
    core.profile_error = None;
    Ok((avatar, banner))
}

/// Hands x76's rank emblems to the UI (missing ones fall back to the drawn diamond).
fn show_rank_icons(app: &AppWindow, icons: Vec<Pixels>) {
    if icons.iter().all(Option::is_none) {
        return;
    }
    let images: Vec<slint::Image> = icons.into_iter().map(|px| px.map(slint::Image::from_rgba8).unwrap_or_default()).collect();
    let g = app.global::<RankIcons>();
    g.set_icons(slint::ModelRc::new(slint::VecModel::from(images)));
    g.set_ready(true);
}

/// Loads the rank emblems in the background (cache first, x76.gg if missing).
fn load_rank_icons(app: &AppWindow, shared: &Shared) {
    let cache = shared.lock().data_file.with_file_name("ranks");
    let weak = app.as_weak();
    std::thread::spawn(move || {
        let icons = ranks::load(&cache);
        let _ = weak.upgrade_in_event_loop(move |app| show_rank_icons(&app, icons));
    });
}

fn show_pictures(app: &AppWindow, avatar: Pixels, banner: Pixels) {
    PICS.with(|p| *p.borrow_mut() = (avatar.is_some(), banner.is_some()));
    if let Some(a) = avatar {
        app.set_avatar(slint::Image::from_rgba8(a));
    }
    if let Some(b) = banner {
        app.set_banner(slint::Image::from_rgba8(b));
    }
}

fn link_profile(app: &AppWindow, shared: &Arc<Shared>, handle: String) {
    let handle = profile::clean_handle(&handle);
    if handle.is_empty() {
        return;
    }
    {
        let mut core = shared.lock();
        core.profile_busy = true;
        core.profile_error = None;
    }
    refresh(app, shared);
    let (weak, shared) = (app.as_weak(), shared.clone());
    std::thread::spawn(move || {
        let result = fetch_profile(&shared, &handle);
        let _ = weak.upgrade_in_event_loop(move |app| {
            {
                let mut core = shared.lock();
                core.profile_busy = false;
                if let Err(e) = &result {
                    core.profile_error = Some(e.clone());
                }
            }
            if let Ok((avatar, banner)) = result {
                show_pictures(&app, avatar, banner);
            }
            refresh(&app, &shared);
        });
    });
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
    app.set_servers(view::servers_page(&core));
    let (has_avatar, has_banner) = PICS.with(|p| *p.borrow());
    app.set_profile(view::profile_card(&core, has_avatar, has_banner));
    let open_detail = UI.with(|ui| ui.borrow().detail.clone());
    app.set_detail(open_detail.and_then(|(key, id)| view::event_detail(&core, &key, &id)).unwrap_or_default());
    app.set_period(period.index());
    app.set_interval_idx(INTERVALS.iter().position(|&i| i == core.interval()).unwrap_or(1) as i32);
    set_discord_page(app, &core, false);
    drop(core);
    tick(app, shared);
}

/// The Discord settings page. Its rows are only rebuilt when something other than a custom text
/// changed: rebuilding them while someone types would drop the cursor.
fn set_discord_page(app: &AppWindow, core: &tracker::Core, force: bool) {
    let mut cfg = discord::Config::load(core);
    cfg.custom.clear();
    let favs: Vec<(&str, &str, &str)> = core.state.favorites.iter().map(|f| (f.game.as_str(), f.id.as_str(), f.name.as_str())).collect();
    let sig = format!("{}|{}|{favs:?}", serde_json::to_string(&cfg).unwrap_or_default(), core.discord.token.is_some());
    let changed = DISCORD_SIG.with(|s| {
        let mut s = s.borrow_mut();
        let changed = force || *s != sig;
        *s = sig;
        changed
    });
    if changed {
        app.set_discord(view::discord_page(core));
    }
    app.set_discord_previews(view::discord_previews(core));
}

/// Changes the Discord settings (saved) and the unsaved Discord state together.
fn edit_discord(shared: &Shared, f: impl FnOnce(&mut discord::Config, &mut discord::Live)) {
    let mut core = shared.lock();
    let mut cfg = discord::Config::load(&core);
    f(&mut cfg, &mut core.discord);
    cfg.save(&mut core);
}

/// Reads the widget's layout from Discord in the background, then redraws.
fn read_layout_bg(app: &AppWindow, shared: &Arc<Shared>) {
    let (weak, shared) = (app.as_weak(), shared.clone());
    std::thread::spawn(move || {
        discord::refresh_layout(&shared);
        let _ = weak.upgrade_in_event_loop(move |app| refresh(&app, &shared));
    });
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
    app.set_servers_status(view::servers_status(&core).into());
    let d = view::discord_status(&core);
    app.set_rpc_status(d.rpc.0.into());
    app.set_rpc_state(d.rpc.1);
    app.set_widget_status(d.widget.0.into());
    app.set_widget_state(d.widget.1);
    app.set_layout_status(d.layout.0.into());
    app.set_layout_error(d.layout.1);
    app.set_layout_busy(d.layout.2);
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
                        app.set_banner_text(format!("Couldn't track that player: {e}").into());
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
    actions.on_open_event({
        let (w, s) = ctx!();
        move |key, id| {
            UI.with(|ui| ui.borrow_mut().detail = Some((key.to_string(), id.to_string())));
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_close_event({
        let (w, s) = ctx!();
        move || {
            UI.with(|ui| ui.borrow_mut().detail = None);
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_profile_link({
        let (w, s) = ctx!();
        move |handle| {
            if let Some(app) = w.upgrade() {
                link_profile(&app, &s, handle.to_string());
            }
        }
    });
    actions.on_profile_unlink({
        let (w, s) = ctx!();
        move || {
            {
                let mut core = s.lock();
                core.state.settings.remove("x76Handle");
                let _ = core.save();
                core.profile = None;
                core.profile_error = None;
            }
            PICS.with(|p| *p.borrow_mut() = (false, false));
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_open_x76({
        let (_, s) = ctx!();
        move || {
            if let Some(h) = s.lock().profile.as_ref().map(|p| p.handle.clone()) {
                let _ = std::process::Command::new("explorer").arg(format!("https://x76.gg/u/?h={h}")).spawn();
            }
        }
    });
    actions.on_dismiss_banner({
        let (w, _) = ctx!();
        move || {
            if let Some(app) = w.upgrade() {
                app.set_banner_text("".into());
            }
        }
    });

    // ---------------------------------------------------------- settings: Discord
    actions.on_settings_opened({
        let (w, s) = ctx!();
        move || {
            if let Some(app) = w.upgrade() {
                set_discord_page(&app, &s.lock(), true);
                tick(&app, &s);
            }
        }
    });
    actions.on_rpc({
        let (w, s) = ctx!();
        move |on| {
            edit_discord(&s, |c, live| {
                c.rpc = on;
                live.rpc_error = None;
            });
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_player({
        let (w, s) = ctx!();
        move |game, i| {
            let Some(g) = Game::from_id(&game) else { return };
            let id = s.lock().state.favorites.iter().filter(|f| f.game == g.id()).nth(i.max(0) as usize).map(|f| f.id.clone());
            if let Some(id) = id {
                edit_discord(&s, |c, live| {
                    match g {
                        Game::R5 => c.r5_player = id,
                        Game::Ql => c.ql_player = id,
                    }
                    live.soon = Some(now() + 2.0);
                });
            }
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_discord_id({
        let (w, s) = ctx!();
        move |which, value| {
            let v: String = value.chars().filter(char::is_ascii_digit).collect();
            let mut reread = false;
            edit_discord(&s, |c, live| {
                if which == "app" {
                    if c.app_id != v {
                        // another app: its widget has its own layout
                        c.app_id = v.clone();
                        c.layout.clear();
                        c.statics.clear();
                        c.widget_name.clear();
                        live.sent.clear();
                        live.layout_note = None;
                        reread = discord::valid_id(&v) && live.token.is_some();
                    }
                } else if c.user_id != v {
                    c.user_id = v.clone();
                    live.soon = Some(now() + 2.0);
                }
            });
            if let Some(app) = w.upgrade() {
                if reread {
                    read_layout_bg(&app, &s);
                }
                refresh(&app, &s);
            }
        }
    });
    actions.on_discord_token({
        let (w, s) = ctx!();
        move |t| {
            let token = discord::clean_token(&t);
            let looks_ok = token.len() >= 50 && token.matches('.').count() == 2 && !token.contains(char::is_whitespace);
            let dir = s.lock().data_file.parent().map(Path::to_path_buf);
            let result = if !looks_ok {
                Err("That doesn't look like a bot token. Copy it again from Developer Portal → your app → Bot.".to_string())
            } else {
                dir.ok_or_else(|| "No folder to save it in.".to_string()).and_then(|d| discord::save_token(&d, &token))
            };
            let mut reread = false;
            {
                let mut core = s.lock();
                match result {
                    Ok(()) => {
                        core.discord.token = Some(token);
                        core.discord.error = None;
                        core.discord.layout_note = None;
                        reread = discord::valid_id(&discord::Config::load(&core).app_id);
                    }
                    Err(e) => core.discord.layout_note = Some((true, e)),
                }
            }
            if let Some(app) = w.upgrade() {
                if reread {
                    read_layout_bg(&app, &s);
                }
                refresh(&app, &s);
            }
        }
    });
    actions.on_discord_token_clear({
        let (w, s) = ctx!();
        move || {
            {
                let mut core = s.lock();
                if let Some(d) = core.data_file.parent() {
                    discord::clear_token(d);
                }
                core.discord.token = None;
                core.discord.sent.clear();
            }
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_widget({
        let (w, s) = ctx!();
        move |on| {
            let mut reread = false;
            edit_discord(&s, |c, live| {
                c.widget = on;
                if on {
                    live.soon = Some(now());
                    live.error = None;
                    reread = c.layout.is_empty() && discord::valid_id(&c.app_id) && live.token.is_some();
                }
            });
            if let Some(app) = w.upgrade() {
                if reread {
                    read_layout_bg(&app, &s);
                }
                refresh(&app, &s);
            }
        }
    });
    actions.on_widget_rpc({
        let (w, s) = ctx!();
        move |on| {
            edit_discord(&s, |c, _| c.widget_rpc = on);
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_widget_layout({
        let (w, s) = ctx!();
        move || {
            if let Some(app) = w.upgrade() {
                read_layout_bg(&app, &s);
                tick(&app, &s);
            }
        }
    });
    actions.on_widget_source({
        let (w, s) = ctx!();
        move |key, kind, i| {
            let kind = match kind {
                0 => "tile",
                2 => "image",
                3 => "card",
                4 => "progress",
                _ => "text",
            };
            if let Some((id, _)) = discord::options(kind).into_iter().nth(i.max(0) as usize) {
                edit_discord(&s, |c, live| {
                    c.sources.insert(key.to_string(), id);
                    live.soon = Some(now() + 2.0);
                });
            }
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_widget_custom({
        let (w, s) = ctx!();
        move |key, text| {
            edit_discord(&s, |c, live| {
                c.custom.insert(key.to_string(), text.to_string());
                live.soon = Some(now() + 8.0); // after they stop typing
            });
            if let Some(app) = w.upgrade() {
                tick(&app, &s);
            }
        }
    });
    actions.on_widget_now({
        let (w, s) = ctx!();
        move || {
            s.lock().discord.soon = Some(now());
            if let Some(app) = w.upgrade() {
                tick(&app, &s);
            }
        }
    });
    actions.on_open_url(|url| {
        if url.starts_with("https://discord.com/") {
            let _ = std::process::Command::new("explorer").arg(url.as_str()).spawn();
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
            // quit outright: an opened dropdown can keep Slint's loop alive after the window hides
            let _ = slint::quit_event_loop();
        }
    });
    // the same when Windows asks the window to close (taskbar, Alt+F4)
    app.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
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
        app.set_banner_text(w.as_str().into());
    }
}

// ------------------------------------------------------------------ guards
fn default_data_dir() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("AppData").join("Local")))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = local.join("x76tracker");
    // version 2.0.0 kept history in "x76EloTracker": copy it over on the first start (the old
    // folder is left as it is)
    let old = local.join("x76EloTracker");
    if !dir.exists() && old.join("state.json").is_file() {
        let _ = copy_dir(&old, &dir);
    }
    dir
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else if entry.file_name() != "app.lock" {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
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

    pub const TITLE: &str = "x76tracker";

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
        /// open the detail panel on the newest R5 result that has fights in it
        pub detail: bool,
        /// show the Servers tab
        pub servers_tab: bool,
        /// open Settings on this tab (0 general, 1 Discord widget)
        pub settings: Option<i32>,
        /// click here before the final frame (e.g. to open a dropdown)
        pub click: Option<(f32, f32)>,
    }

    pub fn run(data_file: PathBuf, o: Options) -> Result<(), Box<dyn std::error::Error>> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Offscreen(window.clone()))).map_err(|e| format!("{e:?}"))?;
        let shared = Shared::new(data_file);
        if !o.offline {
            shared.poll_once(&|_| {});
            shared.poll_servers();
        }
        shared.scan_r5(&mut r5log::Tail::default());
        let saved_handle = shared.lock().setting_str("x76Handle");
        let pics = saved_handle.and_then(|h| fetch_profile(&shared, &h).ok());
        let app = AppWindow::new()?;
        wire(&app, &shared);
        startup(&app, &shared);
        app.invoke_set_dark(o.theme.unwrap_or(true));
        if o.servers_tab {
            app.set_tab(1);
        }
        if let Some(t) = o.settings {
            app.set_settings_open(true);
            app.set_settings_tab(t);
        }
        if o.detail {
            let core = shared.lock();
            if let Some(f) = core.state.favorites.iter().find(|f| f.game == "r5") {
                let key = tracker::key_of("r5", &f.id);
                if let Some(h) = core.state.history.get(&key) {
                    let has_fights = |i: usize| core.fights.iter().any(|x| x.t > h[i - 1].t && x.t <= h[i].t);
                    if let Some(i) = (1..h.len()).rev().find(|&i| has_fights(i)) {
                        UI.with(|ui| ui.borrow_mut().detail = Some((key.clone(), view::row_id(h[i].t))));
                    }
                }
            }
        }
        if let Some((avatar, banner)) = pics {
            show_pictures(&app, avatar, banner);
        }
        let cache = shared.lock().data_file.with_file_name("ranks");
        show_rank_icons(&app, ranks::load(&cache));
        refresh(&app, &shared);
        let (w, h) = o.size;
        window.set_size(slint::PhysicalSize::new(w, h));
        app.show()?;
        let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        for pass in 0..3 {
            if pass == 1 {
                if let Some((x, y)) = o.click {
                    let position = slint::LogicalPosition::new(x, y);
                    let button = slint::platform::PointerEventButton::Left;
                    app.window().dispatch_event(WindowEvent::PointerMoved { position });
                    app.window().dispatch_event(WindowEvent::PointerPressed { position, button });
                    app.window().dispatch_event(WindowEvent::PointerReleased { position, button });
                }
            }
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
                detail: args.iter().any(|a| a == "--detail"),
                servers_tab: arg_value(&args, "--tab").as_deref() == Some("servers"),
                settings: arg_value(&args, "--settings").map(|v| if v == "widget" { 1 } else { 0 }),
                click: arg_value(&args, "--click").and_then(|v| pair(&v, ',')),
            },
        );
    }

    // one copy per history folder: a second launch hands off to the first right away
    let _lock = match single_instance(&data_dir) {
        Lock::Busy => {
            win::focus_existing();
            return Ok(());
        }
        lock => lock,
    };

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
    let saved_handle = shared.lock().setting_str("x76Handle");
    if let Some(h) = saved_handle {
        link_profile(&app, &shared, h); // linked earlier: load the card again
    }
    load_rank_icons(&app, &shared);

    let (poller, notify_to, weak) = (shared.clone(), shared.clone(), app.as_weak());
    std::thread::spawn(move || {
        poller.run(move |full| {
            let s = notify_to.clone();
            let _ = weak.upgrade_in_event_loop(move |app| if full { refresh(&app, &s) } else { tick(&app, &s) });
        })
    });

    // follow R5Reloaded's log for opponents (only finds anything if the game mod is on)
    let (r5_shared, r5_weak) = (shared.clone(), app.as_weak());
    std::thread::spawn(move || {
        let mut tail = r5log::Tail::default();
        loop {
            if r5_shared.scan_r5(&mut tail) {
                let s = r5_shared.clone();
                let _ = r5_weak.upgrade_in_event_loop(move |app| refresh(&app, &s));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    // x76's servers: player counts every minute (a tiny request), recorded for the Servers tab
    let (sv_shared, sv_weak) = (shared.clone(), app.as_weak());
    std::thread::spawn(move || loop {
        if sv_shared.poll_servers() {
            let s = sv_shared.clone();
            let _ = sv_weak.upgrade_in_event_loop(move |app| refresh(&app, &s));
        }
        std::thread::sleep(Duration::from_secs(60));
    });

    // Discord Rich Presence: only while a game runs and it's switched on in Settings
    let (rp_shared, rp_weak) = (shared.clone(), app.as_weak());
    std::thread::spawn(move || {
        let s = rp_shared.clone();
        rpc::run(rp_shared, move || {
            let s = s.clone();
            let _ = rp_weak.upgrade_in_event_loop(move |app| tick(&app, &s));
        })
    });

    // Discord profile widget (for apps that have one): new values when they change
    let (wd_shared, wd_weak) = (shared.clone(), app.as_weak());
    std::thread::spawn(move || {
        let ready = {
            let core = wd_shared.lock();
            let c = discord::Config::load(&core);
            c.widget && discord::valid_id(&c.app_id) && core.discord.token.is_some()
        };
        if ready {
            discord::refresh_layout(&wd_shared); // keep the field list in step with the widget editor
            let s = wd_shared.clone();
            let _ = wd_weak.upgrade_in_event_loop(move |app| refresh(&app, &s));
        }
        loop {
            if discord::widget_step(&wd_shared) {
                let s = wd_shared.clone();
                let _ = wd_weak.upgrade_in_event_loop(move |app| tick(&app, &s));
            }
            std::thread::sleep(Duration::from_secs(3));
        }
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
        win::message(&format!("x76tracker hit an error and stopped:\n\n{e}"));
    }
}
