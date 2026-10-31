//! The notification-area icon, on a thread of its own.
//!
//! Everything the menu does is done **here**, with no help from the window:
//! showing and hiding use `ShowWindow` directly, Start scan spawns the same
//! child process the window's button does, and Quit ends the process. That is
//! deliberate - in background mode there is no window to run the work, and a
//! tray menu that only functions while the window is open would defeat the
//! point of having a tray at all.
//!
//! The thread waits on its message queue rather than polling, so an idle
//! Lyricarr sitting in the tray costs nothing; the one-second wake-up is only
//! so the tooltip can follow a running scan.
//!
//! Windows only. `Shared` and the show flag are used on both systems - on
//! Linux the window itself watches for the flag, because there is no tray.

use crate::config::Config;
#[cfg(windows)]
use crate::store::{self, Store};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(windows)]
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
#[cfg(windows)]
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

/// What the window and the tray thread need to agree on.
pub struct Shared {
    /// The menu's Quit: the tray thread ends the process, because the window
    /// may be hidden and therefore unable to close itself.
    pub quit_requested: AtomicBool,
    /// The window has finished on its own and the process is unwinding.
    pub shutting_down: AtomicBool,
    /// Set once the icon has actually been taken out of the notification area.
    pub tray_gone: AtomicBool,
    /// Mirrors the Settings checkbox. The tray thread is what acts on it,
    /// because the window cannot: a minimised window gets no WM_PAINT, so
    /// eframe never paints the frame that would notice it had been minimised.
    pub minimize_to_tray: AtomicBool,
}

impl Shared {
    pub fn new(minimize_to_tray: bool) -> Arc<Self> {
        Arc::new(Self {
            quit_requested: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            tray_gone: AtomicBool::new(false),
            minimize_to_tray: AtomicBool::new(minimize_to_tray),
        })
    }

    /// Nothing left to wait for - the icon is gone, or there was never a tray.
    pub fn tray_is_gone(&self) -> bool {
        self.tray_gone.load(Ordering::Relaxed)
    }
}

/// A file a second instance drops to say "the user clicked the shortcut again".
pub fn show_flag_path(cfg: &Config) -> PathBuf {
    data_dir(cfg).join("show.flag")
}

fn data_dir(cfg: &Config) -> PathBuf {
    Path::new(&cfg.run.database)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(Config::here)
}

#[cfg(windows)]
fn icon() -> Option<Icon> {
    // The same artwork the window and the exe use, decoded once at startup.
    let bytes = include_bytes!("../assets/lyricarr64.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).ok()
}

#[cfg(windows)]
fn start_scan() {
    let _ = crate::os::lyricarr().arg("scan").arg("--quiet").spawn();
}

#[cfg(windows)]
fn stop_scan(cfg: &Config) {
    let _ = std::fs::write(crate::scan::stop_flag_path(cfg), "stop");
}

#[cfg(windows)]
/// Is a scan running? The same heartbeat the window reads, so the two never
/// disagree about it.
fn scan_is_live(cfg: &Config) -> bool {
    Store::open_readonly(Path::new(&cfg.run.database))
        .ok()
        .and_then(|db| db.get_meta("scan_heartbeat"))
        .and_then(|s| store::parse_iso(&s))
        .map(|t| (chrono::Utc::now() - t).num_seconds() < 60)
        .unwrap_or(false)
}

#[cfg(windows)]
/// Is a scan alive, and how far has it got?
///
/// Takes the connection rather than opening one: this runs on a timer for the
/// life of the process, and re-opening the database each time meant a cold page
/// cache on every call.
fn progress(db: &mut Option<Store>, cfg: &Config, monitor: &crate::watch::Monitor) -> (String, bool) {
    if db.is_none() {
        *db = Store::open_readonly(Path::new(&cfg.run.database)).ok();
    }
    let Some(db) = db.as_ref() else {
        return ("Lyricarr".into(), false);
    };
    let live = db
        .get_meta("scan_heartbeat")
        .and_then(|s| store::parse_iso(&s))
        .map(|t| (chrono::Utc::now() - t).num_seconds() < 60)
        .unwrap_or(false);
    let counts = db.counts().unwrap_or_default();
    let total: i64 = counts.values().sum();
    let done: i64 = [store::ST_SYNCED, store::ST_PLAIN, store::ST_INSTRUMENTAL]
        .iter()
        .map(|k| counts.get(*k).copied().unwrap_or(0))
        .sum();
    let pct = if total > 0 { 100.0 * done as f64 / total as f64 } else { 0.0 };
    let mut tip = if live {
        format!("Lyricarr - scanning\n{done} of {total} ({pct:.0}%)")
    } else {
        format!("Lyricarr - idle\n{done} of {total} have lyrics ({pct:.0}%)")
    };
    // The tooltip is capped at 127 characters by Windows; this line is short.
    let m = monitor.status();
    if m.checking > 0 {
        tip.push_str(&format!("\nlooking up {} new", m.checking));
    } else if m.watch && m.watching() > 0 {
        let n = m.watching();
        tip.push_str(&format!("\nwatching {n} folder{} for new music", if n == 1 { "" } else { "s" }));
    }
    // A busy monitor is a reason to keep the tooltip current, like a scan.
    (tip, live || m.checking > 0 || m.waiting > 0)
}

#[cfg(windows)]
/// Start the tray. Returns immediately; the icon lives on the spawned thread
/// for the rest of the process's life.
pub fn spawn(cfg: Config, shared: Arc<Shared>, monitor: Arc<crate::watch::Monitor>) {
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || run(cfg, shared, monitor))
        .ok();
}

#[cfg(windows)]
fn run(cfg: Config, shared: Arc<Shared>, monitor: Arc<crate::watch::Monitor>) {
    // Whatever happens below, nobody should be left waiting on an icon that
    // was never created.
    struct Mark(Arc<Shared>);
    impl Drop for Mark {
        fn drop(&mut self) {
            self.0.tray_gone.store(true, Ordering::Relaxed);
        }
    }
    let _mark = Mark(shared.clone());

    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open Lyricarr", true, None);
    let hide = MenuItem::with_id("hide", "Hide to tray", true, None);
    let scan = MenuItem::with_id("scan", "Start scan now", true, None);
    let stop = MenuItem::with_id("stop", "Stop scan", true, None);
    let logs = MenuItem::with_id("log", "Open log file", true, None);
    let music = MenuItem::with_id("music", "Open music folder", true, None);
    let quit = MenuItem::with_id("quit", "Quit Lyricarr", true, None);
    let sep = PredefinedMenuItem::separator();
    let sep2 = PredefinedMenuItem::separator();
    let _ = menu.append_items(&[
        &open, &hide, &sep, &scan, &stop, &sep2, &logs, &music, &quit,
    ]);

    let tray = TrayIconBuilder::new()
        .with_tooltip("Lyricarr")
        .with_menu(Box::new(menu))
        // Left click opens the program, right click opens the menu. tray-icon
        // defaults this to true, which put the menu up over the window the
        // same click was asking for.
        .with_menu_on_left_click(false)
        .with_icon(match icon() {
            Some(i) => i,
            // No icon means no tray icon at all on Windows, so there is nothing
            // useful to do but give up quietly and leave the window working.
            None => return,
        })
        .build();
    let Ok(tray) = tray else { return };

    // A left click on the icon is the fastest way back to the window, which is
    // what people expect from a tray app. Unconditionally - if the window is
    // already open but buried, raising it is still the right answer.
    TrayIconEvent::set_event_handler(Some(|ev: TrayIconEvent| {
        let left_up = matches!(
            ev,
            TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } | TrayIconEvent::DoubleClick {
                button: tray_icon::MouseButton::Left,
                ..
            }
        );
        if left_up {
            crate::winui::show();
        }
    }));

    let cfg_for_menu = cfg.clone();
    let shared_for_menu = shared.clone();
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        act(&ev.id, &cfg_for_menu, &shared_for_menu);
    }));

    let flag = show_flag_path(&cfg);
    let mut last_tip = String::new();
    // One connection for the life of the thread. Opening a new one each tick
    // gave it a cold page cache every time, which is what turned an 11ms query
    // into 26-400ms and an idle tray into 7.7% of a core.
    let mut db: Option<Store> = None;
    let mut last_poll = std::time::Instant::now() - std::time::Duration::from_secs(60);
    let mut last_tip_at = last_poll;
    let mut tip_every = std::time::Duration::from_secs(2);
    let poll_every = std::time::Duration::from_millis(750);

    loop {
        // 250ms rather than a second: this is what decides how long a window
        // sits visibly in the taskbar before minimise-to-tray takes it away,
        // and the wake itself is two Win32 calls. The costly work is behind
        // its own gate further down.
        if !crate::winui::pump_for(250) {
            take_icon_down(&tray, &shared);
            return;
        }

        // Going away. The notification area only drops an icon when its owner
        // asks, so both exit paths have to ask - a process that simply ends
        // leaves a dead icon sitting there, and four of them had piled up.
        if shared.quit_requested.load(Ordering::Relaxed)
            || shared.shutting_down.load(Ordering::Relaxed)
        {
            take_icon_down(&tray, &shared);
            if shared.quit_requested.load(Ordering::Relaxed) {
                // Give a window that is actually on screen a moment to close
                // itself; a hidden one cannot, so end the process either way.
                std::thread::sleep(std::time::Duration::from_millis(300));
                std::process::exit(0);
            }
            return;
        }

        // "Minimising hides to the tray" is enforced HERE, not in the window.
        // A minimised window gets no WM_PAINT, so eframe never paints a frame
        // in which egui could notice it had been minimised - the check used to
        // live in App::update and simply never ran. This thread is pumping
        // messages either way, and two cheap Win32 calls answer the question
        // outright. Left above the poll gate on purpose: anything slower than
        // immediate shows up as the window sitting in the taskbar first.
        if shared.minimize_to_tray.load(Ordering::Relaxed)
            && crate::winui::is_minimized()
            && !crate::winui::is_hidden()
        {
            crate::winui::hide();
        }

        // pump_for's argument is a CEILING, not a period: it returns the moment
        // any message arrives. Hovering the tray icon produces a message per
        // mouse sample, and tray-icon arms a 15ms timer on each one - so
        // without this gate the loop body ran dozens of times a second.
        if last_poll.elapsed() < poll_every {
            continue;
        }
        last_poll = std::time::Instant::now();

        // A second copy of the program asks for the window this way rather than
        // opening one of its own. One stat() is cheap enough to do often.
        if flag.is_file() {
            let _ = std::fs::remove_file(&flag);
            crate::winui::show();
        }

        // The tooltip is a convenience, not a readout: two seconds behind
        // during a scan and fifteen when nothing is happening is plenty, and
        // the difference is the whole cost of sitting in the tray.
        if last_tip_at.elapsed() >= tip_every {
            last_tip_at = std::time::Instant::now();
            let (tip, live) = progress(&mut db, &cfg, &monitor);
            tip_every = std::time::Duration::from_secs(if live { 2 } else { 15 });
            if tip != last_tip {
                let _ = tray.set_tooltip(Some(&tip));
                last_tip = tip;
            }
        }
    }
}

#[cfg(windows)]
/// Remove the icon from the notification area and say so.
fn take_icon_down(tray: &tray_icon::TrayIcon, shared: &Arc<Shared>) {
    let _ = tray.set_visible(false);
    shared.tray_gone.store(true, Ordering::Relaxed);
}

#[cfg(windows)]
fn act(id: &MenuId, cfg: &Config, shared: &Arc<Shared>) {
    match id.as_ref() {
        "open" => {
            crate::winui::show();
        }
        "hide" => {
            crate::winui::hide();
        }
        "scan" => {
            // Without this the menu item happily starts a second scanner on top
            // of a running one. The scan itself now refuses to be the second
            // full pass, but there is no reason to spawn a process just to have
            // it exit, and the heartbeat is right here.
            if scan_is_live(cfg) {
                return;
            }
            start_scan()
        }
        "stop" => stop_scan(cfg),
        "log" => {
            let _ = opener::open(&cfg.run.log_file);
        }
        "music" => {
            if let Some(root) = cfg.library.roots.first() {
                let _ = opener::open(root);
            }
        }
        "quit" => {
            // Just raise the flag. The pump loop is what ends the process, so
            // that the icon comes out of the notification area first -
            // calling process::exit(0) from here ran no destructors and left
            // the icon behind. Preferences are written the moment they
            // change, so there is nothing else to flush.
            shared.quit_requested.store(true, Ordering::SeqCst);
        }
        _ => {}
    }
}
