//! x76tracker. A window drawn with Slint; a background thread checks the
//! x76 ladders and records every change for the players you track.
//!
//!   x76tracker.exe                            normal start
//!   x76tracker.exe --data-dir D:\x76          keep history somewhere else
//!   x76tracker.exe --screenshot out.png [--size 1440x1000] [--light|--dark] [--hover x,y] [--offline]
//!                  [--tab servers] [--settings general|widget|updates] [--preview-update 2.3.0]
//!                  [--whats-new N] [--record FRAMES [--fps 20] [--crop x,y,w,h] [--click-at F] [--hover-at F]]
//!                                             (--record writes an animated PNG)
//!   x76tracker.exe --card out.png             the share card alone, as a PNG
//!                                             render the window to a PNG without showing it
#![windows_subsystem = "windows"]

slint::include_modules!();

mod discord;
mod profile;
mod qllog;
mod r5log;
mod r5patch;
mod ranks;
mod rpc;
mod servers;
mod tracker;
mod update;
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
    // What's new pictures, decoded (cleared when the menu closes)
    static MEDIA: RefCell<std::collections::HashMap<String, MediaSlot>> = RefCell::new(std::collections::HashMap::new());
}

enum MediaSlot {
    Loading,
    Ready { frames: Vec<slint::Image>, frame_ms: i32, aspect: f32, natural: f32 },
    Failed,
}

/// What the menu shows for a picture right now.
fn media_view(m: &update::Media) -> ChangeMedia {
    let alt: slint::SharedString = m.alt.as_str().into();
    MEDIA.with(|map| match map.borrow().get(&m.src) {
        Some(MediaSlot::Ready { frames, frame_ms, aspect, natural }) => ChangeMedia {
            has: true,
            ready: true,
            failed: false,
            alt,
            frames: slint::ModelRc::new(slint::VecModel::from(frames.clone())),
            frame_ms: *frame_ms,
            aspect: *aspect,
            natural: *natural,
        },
        Some(MediaSlot::Failed) => ChangeMedia { has: true, failed: true, alt, ..Default::default() },
        _ => ChangeMedia { has: true, alt, ..Default::default() },
    })
}

fn store_media(src: &str, result: Result<update::Frames, String>) {
    let slot = match result {
        Ok(f) => {
            let images: Vec<slint::Image> =
                f.frames.iter().map(|px| slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(px, f.width, f.height))).collect();
            // "…@2x.png" has two pixels per point: shown at half its pixel size, so it stays sharp
            let density = if src.contains("@2x") { 2.0 } else { 1.0 };
            MediaSlot::Ready {
                frames: f.order.iter().map(|&i| images[i].clone()).collect(),
                frame_ms: f.frame_ms as i32,
                aspect: if f.width > 0 { f.height as f32 / f.width as f32 } else { 0.5 },
                natural: f.width as f32 / density,
            }
        }
        Err(_) => MediaSlot::Failed,
    };
    MEDIA.with(|m| m.borrow_mut().insert(src.to_string(), slot));
}

/// Loads every picture the What's new menu shows: in the background, or right away (`wait`).
fn load_changelog_media(app: &AppWindow, shared: &Arc<Shared>, wait: bool) {
    let (list, cache) = {
        let core = shared.lock();
        let mut list: Vec<update::Media> = Vec::new();
        if let Some(r) = core.updater.newer() {
            for e in update::parse_changelog(&r.notes, &r.version).into_iter().take(1) {
                list.extend(e.media().cloned());
            }
        }
        for e in update::parse_changelog(update::CHANGELOG, "") {
            list.extend(e.media().cloned());
        }
        (list, core.data_file.with_file_name("changelog"))
    };
    for m in list {
        let fresh = MEDIA.with(|map| {
            let mut map = map.borrow_mut();
            if map.contains_key(&m.src) {
                return false;
            }
            map.insert(m.src.clone(), MediaSlot::Loading);
            true
        });
        if !fresh {
            continue;
        }
        let (src, cache) = (m.src.clone(), cache.clone());
        let decode = move || update::media_bytes(&src, &cache).and_then(|b| update::decode_media(&b));
        if wait {
            store_media(&m.src, decode());
        } else {
            let (weak, s, src) = (app.as_weak(), shared.clone(), m.src.clone());
            std::thread::spawn(move || {
                let result = decode();
                let _ = weak.upgrade_in_event_loop(move |app| {
                    store_media(&src, result);
                    refresh(&app, &s);
                });
            });
        }
    }
    refresh(app, shared);
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
        let ql = view::game_data(&core, Game::Ql, period, &ui);
        let r5 = view::game_data(&core, Game::R5, period, &ui);
        // remember what's on the boards now: next time only new results fade in, and numbers tick from here
        for g in [&ql, &r5] {
            use slint::Model;
            for c in g.players.iter() {
                let ids = ui.seen.entry(c.key.to_string()).or_default();
                ids.extend(c.events.iter().map(|e| e.id.to_string()).filter(|id| !id.is_empty()));
                if c.has_num {
                    ui.shown_nums.insert(c.key.to_string(), (c.points_num as f64, c.elo_num as f64));
                }
            }
        }
        app.set_ql(ql);
        app.set_r5(r5);
        ui.flash.clear();
    });
    app.set_servers(view::servers_page(&core));
    app.set_updates(view::updates_page(&core));
    app.set_changelog(slint::ModelRc::new(slint::VecModel::from(view::changelog(&core, &media_view))));
    let (has_avatar, has_banner) = PICS.with(|p| *p.borrow());
    app.set_profile(view::profile_card(&core, has_avatar, has_banner));
    app.set_share(view::share_card(&core, has_avatar, has_banner));
    let open_rival = UI.with(|ui| ui.borrow().rival.clone());
    app.set_rival(open_rival.and_then(|(key, opp)| view::rival_page(&core, &key, &opp)).unwrap_or_default());
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
    let (t, st) = view::patch_status(&core);
    app.set_patch_status(t.into());
    app.set_patch_state(st);
    let (t, st) = view::ql_log_status(&core);
    app.set_qllog_status(t.into());
    app.set_qllog_state(st);
    let (t, st) = view::update_status(&core);
    app.set_update_status(t.into());
    app.set_update_state(st);
    app.set_card_busy(core.share_busy);
    app.set_card_saved(core.share_saved.is_some());
    let (err, note) = core.share_note.clone().unwrap_or_default();
    app.set_card_status(note.into());
    app.set_card_status_error(err);
}

/// Where share cards are saved: Pictures\x76tracker (or next to the history if there's no Pictures).
fn cards_dir(data_dir: &Path) -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|h| PathBuf::from(h).join("Pictures"))
        .filter(|p| p.is_dir())
        .map(|p| p.join("x76tracker"))
        .unwrap_or_else(|| data_dir.join("cards"))
}

/// Opens Explorer with the file selected.
fn reveal(file: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", file.display())).spawn();
    }
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
    actions.on_open_rival({
        let (w, s) = ctx!();
        move |key, opponent| {
            UI.with(|ui| ui.borrow_mut().rival = Some((key.to_string(), opponent.to_string())));
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_close_rival({
        let (w, s) = ctx!();
        move || {
            UI.with(|ui| ui.borrow_mut().rival = None);
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
    actions.on_r5_patch({
        let (w, s) = ctx!();
        move |on| {
            let dir = s.lock().setting_str("r5Path").map(PathBuf::from);
            let result = match &dir {
                None => Err("Enter the R5Reloaded folder first.".to_string()),
                Some(d) if on => r5patch::install(d),
                Some(d) => r5patch::remove(d),
            };
            {
                let mut core = s.lock();
                match result {
                    Ok(()) => {
                        core.set_setting("r5Patch", on.into());
                        core.r5patch_note = None;
                    }
                    Err(e) => core.r5patch_note = Some((true, e)),
                }
            }
            s.check_r5_patch(false);
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_ql_log({
        let (w, s) = ctx!();
        move |on| {
            let dir = s.lock().setting_str("qlPath").map(PathBuf::from);
            let result = match &dir {
                None => Err("Quake Live wasn't found. Start it once, then try again.".to_string()),
                Some(d) if on => qllog::install(d),
                Some(d) => qllog::remove(d),
            };
            {
                let mut core = s.lock();
                core.qllog_note = match result {
                    Ok(()) if on => Some((false, r"In Quake Live's console, type \set logfile 2 to start now, or restart the game.".to_string())),
                    Ok(()) => None,
                    Err(e) => Some((true, e)),
                };
            }
            s.check_ql_log();
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_r5_path({
        let (w, s) = ctx!();
        move |text| {
            let dir = PathBuf::from(text.trim().trim_matches('"'));
            if dir.join("r5apex.exe").is_file() {
                s.lock().set_setting("r5Path", dir.display().to_string().into());
                s.check_r5_patch(false);
                if let Some(app) = w.upgrade() {
                    refresh(&app, &s);
                }
            }
        }
    });
    actions.on_card_opened({
        let (w, s) = ctx!();
        move || {
            if s.lock().share_busy {
                // keep showing the save in progress
            } else {
                s.lock().share_note = None;
            }
            if let Some(app) = w.upgrade() {
                app.set_card_open(true);
                refresh(&app, &s);
            }
        }
    });
    actions.on_card_save({
        let (w, s) = ctx!();
        move || {
            let (data_dir, name) = {
                let mut core = s.lock();
                if core.share_busy {
                    return;
                }
                core.share_busy = true;
                core.share_note = None;
                let name = core
                    .profile
                    .as_ref()
                    .map(|p| p.handle.clone())
                    .or_else(|| core.state.favorites.first().map(|f| f.name.clone()))
                    .unwrap_or_else(|| "card".into());
                (core.data_file.parent().map(Path::to_path_buf).unwrap_or_default(), name)
            };
            if let Some(app) = w.upgrade() {
                tick(&app, &s);
            }
            let (w, s) = (w.clone(), s.clone());
            std::thread::spawn(move || {
                // drawn by this same exe in its offscreen mode, at twice the size for a crisp image
                let safe: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
                let file = cards_dir(&data_dir).join(format!("x76tracker-{safe}-{}.png", chrono::Local::now().format("%Y-%m-%d-%H%M")));
                let result = std::fs::create_dir_all(file.parent().unwrap_or(&data_dir))
                    .map_err(|e| e.to_string())
                    .and_then(|_| std::env::current_exe().map_err(|e| e.to_string()))
                    .and_then(|exe| {
                        std::process::Command::new(exe)
                            .arg("--data-dir")
                            .arg(&data_dir)
                            .arg("--card")
                            .arg(&file)
                            .status()
                            .map_err(|e| e.to_string())
                    })
                    .and_then(|st| if st.success() && file.is_file() { Ok(()) } else { Err("the card couldn't be drawn".to_string()) });
                {
                    let mut core = s.lock();
                    core.share_busy = false;
                    match result {
                        Ok(()) => {
                            core.share_note = Some((false, format!("Saved to {}", file.display())));
                            core.share_saved = Some(file.clone());
                        }
                        Err(e) => core.share_note = Some((true, format!("Couldn't save the card: {e}"))),
                    }
                }
                if file.is_file() {
                    reveal(&file);
                }
                let _ = w.upgrade_in_event_loop(move |app| tick(&app, &s));
            });
        }
    });
    actions.on_changelog_opened({
        let (w, s) = ctx!();
        move || {
            if let Some(app) = w.upgrade() {
                load_changelog_media(&app, &s, false);
            }
        }
    });
    actions.on_changelog_closed({
        let (w, s) = ctx!();
        move || {
            MEDIA.with(|m| m.borrow_mut().clear()); // decoded animations take memory
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_card_reveal({
        let (_, s) = ctx!();
        move || {
            if let Some(f) = s.lock().share_saved.clone() {
                reveal(&f);
            }
        }
    });
    actions.on_update_check({
        let (w, s) = ctx!();
        move || {
            let (w, s) = (w.clone(), s.clone());
            std::thread::spawn(move || {
                update::check(&s);
                let _ = w.upgrade_in_event_loop(move |app| refresh(&app, &s));
            });
        }
    });
    actions.on_update_install({
        let (w, s) = ctx!();
        move || {
            let (w, s) = (w.clone(), s.clone());
            std::thread::spawn(move || {
                let notify = {
                    let (w, s) = (w.clone(), s.clone());
                    move || {
                        let s = s.clone();
                        let _ = w.upgrade_in_event_loop(move |app| refresh(&app, &s));
                    }
                };
                if update::fetch_update(&s, &notify) {
                    // the swap happens after the window closes (see finish_update)
                    update::RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = w.upgrade_in_event_loop(|app| {
                        let _ = app.window().hide();
                        let _ = slint::quit_event_loop();
                    });
                }
            });
        }
    });
    actions.on_update_setting({
        let (w, s) = ctx!();
        move |which, on| {
            let key = if which == "check" { "updateCheck" } else { "updateAuto" };
            s.lock().set_setting(key, on.into());
            if let Some(app) = w.upgrade() {
                refresh(&app, &s);
            }
        }
    });
    actions.on_open_url(|url| {
        if url.starts_with("https://discord.com/") || url.starts_with("https://github.com/vocaloidcats/x76tracker") {
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
        // animations run on this clock, so frames come out evenly spaced however long they take to draw
        fn duration_since_start(&self) -> Duration {
            Duration::from_millis(CLOCK_MS.load(std::sync::atomic::Ordering::Relaxed))
        }
    }

    static CLOCK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Moves the clock on in 16 ms steps, so Timer-driven motion (which fires once per step)
    /// keeps real-time speed in screenshots and recordings.
    fn advance(ms: u64) {
        let mut left = ms;
        while left > 0 {
            let d = left.min(16);
            CLOCK_MS.fetch_add(d, std::sync::atomic::Ordering::Relaxed);
            slint::platform::update_timers_and_animations();
            left -= d;
        }
    }

    /// An animated PNG of the window (or part of it), frame by frame.
    pub struct Record {
        pub frames: u32,
        pub fps: u32,
        /// x, y, width, height in window pixels
        pub crop: Option<(u32, u32, u32, u32)>,
        /// frame at which the --click / --hover happens (otherwise before recording starts)
        pub click_at: Option<u32>,
        pub hover_at: Option<u32>,
        /// frame at which new results "arrive": the newest two rows slide in, points and Elo tick
        pub board_at: Option<u32>,
    }

    fn click(app: &AppWindow, (x, y): (f32, f32)) {
        let position = slint::LogicalPosition::new(x, y);
        let button = slint::platform::PointerEventButton::Left;
        app.window().dispatch_event(WindowEvent::PointerMoved { position });
        app.window().dispatch_event(WindowEvent::PointerPressed { position, button });
        app.window().dispatch_event(WindowEvent::PointerReleased { position, button });
    }

    fn hover(app: &AppWindow, (x, y): (f32, f32)) {
        app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(x, y) });
    }

    /// Straight (not premultiplied) RGBA of a region of the frame.
    fn region(buf: &[PremultipliedRgbaColor], stride: u32, (x0, y0, cw, ch): (u32, u32, u32, u32)) -> Vec<u8> {
        let mut out = Vec::with_capacity((cw * ch * 4) as usize);
        for y in y0..y0 + ch {
            for p in &buf[(y * stride + x0) as usize..(y * stride + x0 + cw) as usize] {
                let a = p.alpha as u32;
                let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
                out.extend_from_slice(&[un(p.red), un(p.green), un(p.blue), p.alpha]);
            }
        }
        out
    }

    pub struct Options {
        pub out: String,
        pub size: (u32, u32),
        pub theme: Option<bool>,
        pub hover: Option<(f32, f32)>,
        pub offline: bool,
        /// open the detail panel on the newest result that has fights in it (or an upset win)
        pub detail: bool,
        pub detail_upset: bool,
        /// which game's result that is: "r5" (the default) or "ql"
        pub detail_game: String,
        /// open the head-to-head with the most-fought opponent of this game's first tracked player
        pub rival: Option<String>,
        /// show the Servers tab
        pub servers_tab: bool,
        /// open Settings on this tab (0 general, 1 Experimental: Discord, 2 updates)
        pub settings: Option<i32>,
        /// click here before the final frame (e.g. to open a dropdown)
        pub click: Option<(f32, f32)>,
        /// pretend this version is the latest release, to see the update screens
        pub preview_update: Option<String>,
        /// draw only the share card (at 2x), for the PNG export
        pub card: bool,
        /// open What's new on this entry (its pictures are loaded first)
        pub whats_new: Option<i32>,
        /// write an animated PNG instead of a still
        pub record: Option<Record>,
        /// pixels per point (2 = twice the resolution; size, clicks and crops stay in points)
        pub scale: f32,
    }

    pub fn run(data_file: PathBuf, o: Options) -> Result<(), Box<dyn std::error::Error>> {
        // X76_PARTIAL=1 redraws only what changed, like a real window does (for checking that)
        let repaint = if std::env::var_os("X76_PARTIAL").is_some() { RepaintBufferType::ReusedBuffer } else { RepaintBufferType::NewBuffer };
        let window = MinimalSoftwareWindow::new(repaint);
        slint::platform::set_platform(Box::new(Offscreen(window.clone()))).map_err(|e| format!("{e:?}"))?;
        let shared = Shared::new(data_file);
        if !o.offline {
            shared.poll_once(&|_| {});
            shared.poll_servers();
            update::check(&shared);
        }
        if let Some(v) = &o.preview_update {
            update::PREVIEW.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut core = shared.lock();
            core.updater.latest = Some(update::preview_release(v));
            core.updater.checked_at = Some(now());
            core.updater.error = None;
        }
        shared.scan_r5(&mut r5log::Tail::default());
        shared.check_r5_patch(false);
        shared.scan_ql(&mut qllog::Tail::default());
        shared.check_ql_log();
        let saved_handle = shared.lock().setting_str("x76Handle");
        let pics = if o.card {
            // the card only needs the profile and its pictures, not fresh ladders
            saved_handle.and_then(|h| profile::fetch(&h).ok()).map(|p| {
                let avatar = p.avatar_url.as_deref().and_then(|u| profile::picture(u, 256, true));
                let banner = p.banner_url.as_deref().and_then(|u| profile::picture(u, 1600, false));
                shared.lock().profile = Some(p);
                (avatar, banner)
            })
        } else {
            saved_handle.and_then(|h| fetch_profile(&shared, &h).ok())
        };
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
        if o.card {
            app.set_card_mode(true);
        }
        let scale = o.scale.clamp(0.5, 4.0);
        if scale != 1.0 {
            app.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
        }
        if o.detail || o.detail_upset {
            let core = shared.lock();
            let game = o.detail_game.as_str();
            let fights = if game == "ql" { &core.ql_fights } else { &core.fights };
            if let Some(f) = core.state.favorites.iter().find(|f| f.game == game) {
                let key = tracker::key_of(game, &f.id);
                if let Some(h) = core.state.history.get(&key) {
                    let upset_win = |x: &r5log::Fight| {
                        x.kind == "kill" && x.opp_elo.zip(x.my_elo).is_some_and(|(o, m)| 1.0 / (1.0 + 10f64.powf((o - m) / 400.0)) < 0.35)
                    };
                    let has_fights = |i: usize| {
                        fights.iter().any(|x| x.me.eq_ignore_ascii_case(&f.id) && x.t > h[i - 1].t && x.t <= h[i].t && (!o.detail_upset || upset_win(x)))
                    };
                    if let Some(i) = (1..h.len()).rev().find(|&i| has_fights(i)) {
                        UI.with(|ui| ui.borrow_mut().detail = Some((key.clone(), view::row_id(h[i].t))));
                    }
                }
            }
        }
        if let Some(game) = &o.rival {
            let core = shared.lock();
            let fav = core.state.favorites.iter().find(|f| &f.game == game);
            let fights = if game == "ql" { &core.ql_fights } else { &core.fights };
            if let Some(f) = fav {
                let mine: Vec<&r5log::Fight> = fights.iter().filter(|x| x.me.eq_ignore_ascii_case(&f.id)).collect();
                if let Some(top) = view::records(&mine).first() {
                    UI.with(|ui| ui.borrow_mut().rival = Some((tracker::key_of(game, &f.id), top.name.clone())));
                }
            }
        }
        if let Some((avatar, banner)) = pics {
            show_pictures(&app, avatar, banner);
        }
        let cache = shared.lock().data_file.with_file_name("ranks");
        show_rank_icons(&app, ranks::load(&cache));
        refresh(&app, &shared);
        if let Some(i) = o.whats_new {
            load_changelog_media(&app, &shared, true);
            app.set_changelog_index(i);
            app.set_changelog_open(true);
        }
        let (w, h) = ((o.size.0 as f32 * scale).round() as u32, (o.size.1 as f32 * scale).round() as u32);
        window.set_size(slint::PhysicalSize::new(w, h));
        app.show()?;
        let rec = o.record.as_ref();
        let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        // a few passes to let layouts, pictures and opening animations settle
        for pass in 0..3 {
            if pass == 1 && rec.map_or(true, |r| r.click_at.is_none()) {
                if let Some(at) = o.click {
                    click(&app, at);
                }
            }
            if pass == 2 && rec.map_or(true, |r| r.hover_at.is_none()) {
                if let Some(at) = o.hover {
                    hover(&app, at);
                }
            }
            advance(700);
            window.request_redraw();
            window.draw_if_needed(|r| {
                r.render(&mut buf, w as usize);
            });
        }
        if let Some(r) = rec {
            let step = (1000 / r.fps.clamp(1, 60)) as u64;
            let px = |v: u32| (v as f32 * scale).round() as u32;
            let (cx, cy, cw, ch) = r.crop.map(|(x, y, cw, ch)| (px(x), px(y), px(cw), px(ch))).unwrap_or((0, 0, w, h));
            let crop = (cx.min(w - 1), cy.min(h - 1), cw.min(w - cx.min(w - 1)), ch.min(h - cy.min(h - 1)));
            let mut frames = Vec::with_capacity(r.frames as usize);
            for i in 0..r.frames {
                if r.click_at == Some(i) {
                    if let Some(at) = o.click {
                        click(&app, at);
                    }
                }
                if r.hover_at == Some(i) {
                    if let Some(at) = o.hover {
                        hover(&app, at);
                    }
                }
                if r.board_at == Some(i) {
                    use slint::Model;
                    let games = [app.get_ql(), app.get_r5()];
                    UI.with(|ui| {
                        let mut ui = ui.borrow_mut();
                        for g in &games {
                            for c in g.players.iter() {
                                let key = c.key.to_string();
                                if c.has_num {
                                    ui.shown_nums.insert(key.clone(), (c.points_num as f64 - 420.0, c.elo_num as f64 - 14.6));
                                }
                                let newest: Vec<String> = c.events.iter().map(|e| e.id.to_string()).filter(|id| !id.is_empty()).take(2).collect();
                                if let Some(seen) = ui.seen.get_mut(&key) {
                                    for id in newest {
                                        seen.remove(&id);
                                    }
                                }
                            }
                        }
                    });
                    refresh(&app, &shared);
                }
                advance(step);
                window.request_redraw();
                window.draw_if_needed(|rd| {
                    rd.render(&mut buf, w as usize);
                });
                frames.push(region(&buf, w, crop));
            }
            // identical frames in a row become one frame shown longer (a still moment costs nothing)
            let mut runs: Vec<(Vec<u8>, u32)> = Vec::new();
            for f in frames {
                match runs.last_mut() {
                    Some((last, n)) if *last == f => *n += 1,
                    _ => runs.push((f, 1)),
                }
            }
            let file = std::io::BufWriter::new(std::fs::File::create(&o.out)?);
            let mut enc = png::Encoder::new(file, crop.2, crop.3);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_animated(runs.len() as u32, 0)?; // loop forever
            let mut writer = enc.write_header()?;
            // after the first frame, store only the rectangle that changed (drawn over the last frame)
            let (fw, fh) = (crop.2, crop.3);
            let mut prev: Option<&Vec<u8>> = None;
            for (f, n) in &runs {
                writer.set_frame_delay((step as u32 * n).min(u16::MAX as u32) as u16, 1000)?;
                let Some(p) = prev else {
                    writer.write_image_data(f)?;
                    prev = Some(f);
                    continue;
                };
                let (mut x0, mut y0, mut x1, mut y1) = (fw, fh, 0, 0);
                for y in 0..fh {
                    for x in 0..fw {
                        let i = ((y * fw + x) * 4) as usize;
                        if f[i..i + 4] != p[i..i + 4] {
                            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                        }
                    }
                }
                if x1 < x0 || y1 < y0 {
                    (x0, y0, x1, y1) = (0, 0, 0, 0);
                }
                let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
                let mut sub = Vec::with_capacity((w * h * 4) as usize);
                for y in y0..=y1 {
                    let start = ((y * fw + x0) * 4) as usize;
                    sub.extend_from_slice(&f[start..start + (w * 4) as usize]);
                }
                writer.reset_frame_position()?;
                writer.set_frame_dimension(w, h)?;
                writer.set_frame_position(x0, y0)?;
                writer.set_blend_op(png::BlendOp::Source)?;
                writer.set_dispose_op(png::DisposeOp::None)?;
                writer.write_image_data(&sub)?;
                prev = Some(f);
            }
            writer.finish()?;
            return Ok(());
        }
        let rgba = region(&buf, w, (0, 0, w, h));
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

#[cfg(feature = "docs")]
fn pair<T: std::str::FromStr>(v: &str, sep: char) -> Option<(T, T)> {
    let (a, b) = v.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = arg_value(&args, "--data-dir").map(PathBuf::from).unwrap_or_else(default_data_dir);
    let data_file = data_dir.join("state.json");

    #[cfg(feature = "docs")]
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
                detail_upset: args.iter().any(|a| a == "--detail-upset"),
                detail_game: arg_value(&args, "--detail-game").unwrap_or_else(|| "r5".into()),
                rival: arg_value(&args, "--rival"),
                servers_tab: arg_value(&args, "--tab").as_deref() == Some("servers"),
                settings: arg_value(&args, "--settings").map(|v| match v.as_str() {
                    "experimental" | "widget" => 1,
                    "updates" => 2,
                    _ => 0,
                }),
                click: arg_value(&args, "--click").and_then(|v| pair(&v, ',')),
                preview_update: arg_value(&args, "--preview-update"),
                card: false,
                whats_new: arg_value(&args, "--whats-new").and_then(|v| v.parse().ok()),
                record: arg_value(&args, "--record").and_then(|v| v.parse().ok()).map(|frames| screenshot::Record {
                    frames,
                    fps: arg_value(&args, "--fps").and_then(|v| v.parse().ok()).unwrap_or(20),
                    crop: arg_value(&args, "--crop").and_then(|v| {
                        let n: Vec<u32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                        (n.len() == 4).then(|| (n[0], n[1], n[2], n[3]))
                    }),
                    click_at: arg_value(&args, "--click-at").and_then(|v| v.parse().ok()),
                    hover_at: arg_value(&args, "--hover-at").and_then(|v| v.parse().ok()),
                    board_at: arg_value(&args, "--board-at").and_then(|v| v.parse().ok()),
                }),
                scale: arg_value(&args, "--scale").and_then(|v| v.parse().ok()).unwrap_or(1.0),
            },
        );
    }
    // the share card alone, as a PNG (Save PNG runs this; it reads the saved history, no checks)
    if let Some(out) = arg_value(&args, "--card") {
        return screenshot::run(
            data_file,
            screenshot::Options {
                out,
                size: (640, 336),
                theme: Some(true),
                hover: None,
                offline: true,
                detail: false,
                detail_upset: false,
                detail_game: String::new(),
                rival: None,
                servers_tab: false,
                settings: None,
                click: None,
                preview_update: None,
                card: true,
                whats_new: None,
                record: None,
                scale: 2.0,
            },
        );
    }

    // one copy per history folder: a second launch hands off to the first right away
    let lock = match single_instance(&data_dir) {
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

    update::clean_up(&data_dir.join("update")); // the old exe and downloads from the last update
    let shared = Shared::new(data_file);
    let app = AppWindow::new()?;
    wire(&app, &shared);
    wire_window(&app);
    startup(&app, &shared);
    // first start of a new version: show what's new
    let last = shared.lock().setting_str("lastVersion");
    if last.as_deref() != Some(VERSION) {
        shared.lock().set_setting("lastVersion", VERSION.into());
        if last.as_deref().is_some_and(|l| update::is_newer(VERSION, l)) {
            app.set_changelog_index(0);
            app.set_changelog_open(true);
            load_changelog_media(&app, &shared, false);
        }
    }
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

    // follow R5Reloaded's log (only finds anything if the game mod is on) and Quake Live's console
    // log (only written while `logfile` is on) for opponents
    let (r5_shared, r5_weak) = (shared.clone(), app.as_weak());
    std::thread::spawn(move || {
        let mut tail = r5log::Tail::default();
        let mut ql_tail = qllog::Tail::default();
        let mut round = 0u32;
        loop {
            let mut changed = r5_shared.scan_r5(&mut tail);
            changed |= r5_shared.scan_ql(&mut ql_tail);
            // the patch: checked now and then, and put back if a game update replaced the script
            if round % 30 == 0 {
                changed |= r5_shared.check_r5_patch(true);
                changed |= r5_shared.check_ql_log();
            }
            round = round.wrapping_add(1);
            if changed {
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

    // updates: check GitHub now and then; with automatic install on, download in the background
    let (up_shared, up_weak) = (shared.clone(), app.as_weak());
    std::thread::spawn(move || {
        let notify = {
            let (s, w) = (up_shared.clone(), up_weak.clone());
            move || {
                let s = s.clone();
                let _ = w.upgrade_in_event_loop(move |app| refresh(&app, &s));
            }
        };
        std::thread::sleep(Duration::from_secs(10));
        loop {
            let (due, fetch) = {
                let core = up_shared.lock();
                let due = update::auto_check(&core) && core.updater.checked_at.map_or(true, |t| now() - t >= update::CHECK_EVERY);
                (due, update::auto_install(&core) && !update::dev_build() && core.updater.auto_download_allowed())
            };
            if due && update::check(&up_shared) {
                notify();
            }
            if fetch {
                update::fetch_update(&up_shared, &notify);
            }
            std::thread::sleep(Duration::from_secs(60));
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
    finish_update(&shared, lock);
    Ok(())
}

/// After the window closes: swap in a downloaded update (automatic install, or "Install and
/// restart", which also starts the new version).
fn finish_update(shared: &Shared, lock: Lock) {
    let restart = update::RESTART.load(std::sync::atomic::Ordering::SeqCst);
    let staged = {
        let core = shared.lock();
        let auto = update::auto_install(&core);
        core.updater.staged.clone().filter(|(v, p)| p.is_file() && update::is_newer(v, VERSION) && (restart || auto))
    };
    let (Some((_, path)), Ok(exe)) = (staged, std::env::current_exe()) else { return };
    if update::dev_build() {
        return;
    }
    drop(lock); // let the new version start
    match update::swap_at(&exe, &path) {
        Ok(()) if restart => {
            let _ = std::process::Command::new(&exe).spawn();
        }
        Ok(()) => {}
        Err(e) if restart => win::message(&format!("The update couldn't be installed:\n\n{e}")),
        Err(_) => {}
    }
}

fn main() {
    if let Err(e) = run(std::env::args().collect()) {
        win::message(&format!("x76tracker hit an error and stopped:\n\n{e}"));
    }
}
