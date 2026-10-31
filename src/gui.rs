//! The window.
//!
//! Same job as the old tkinter front end - coverage at a glance, live scan
//! state, a browsable track list and a log tail - but drawn by egui so it
//! ships inside the one executable with no Python and no Tk.
//!
//! It never queries the database on the thread that draws. A `Loader` thread
//! owns a read-only handle, answers the window's latest question, and asks for
//! a repaint when it has; the window just shows the newest answer. The queries
//! used to run inline every three seconds, and on a library of a hundred
//! thousand tracks the sort alone was enough to make the whole window hitch
//! on a beat - and typing in the search box re-ran it on every key.
//!
//! WAL mode lets that reader and the scanner's writer coexist, so the window
//! is safe to leave open while a scan runs. Start/Stop drive a separate
//! `lyricarr scan` process, and Stop touches a flag file the scanner polls so
//! it finishes the tracks already in flight rather than being killed.
//!
//! Writes it does make - a hand-picked match, "mark instrumental", clearing a
//! retry window - open their own short-lived writable handle and close it
//! again, so the window never holds a write transaction open while you read.

use crate::config::Config;
use crate::lrclib::Candidate;
use crate::prefs::Prefs;
use crate::store::{self, Row, SortCol, Store, STATUSES};
use crate::tools;
use crate::watch;
use anyhow::Result;
use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Readability: the first version of this window drew almost everything at 11
// and 12 points, which is what "a little hard to read" meant. Body text is 14
// now, small print is 12, and nothing is below 12 except bold upper-case
// column headers.
const BODY: f32 = 14.0;
const SMALL: f32 = 12.0;
const TINY: f32 = 11.0;

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4a, 0xc0, 0xc0);
const OK: egui::Color32 = egui::Color32::from_rgb(0x52, 0xc4, 0x7a);
const WARN: egui::Color32 = egui::Color32::from_rgb(0xe0, 0xa8, 0x3a);
const BAD: egui::Color32 = egui::Color32::from_rgb(0xe0, 0x6c, 0x5c);
// Lightened from #8a93a6: on this background that was 5.4:1 and it was being
// used for 11pt text. This is 7.5:1.
const MUTED: egui::Color32 = egui::Color32::from_rgb(0x9a, 0xa6, 0xbd);
const INK: egui::Color32 = egui::Color32::from_rgb(0xe8, 0xec, 0xf4);
const CARD: egui::Color32 = egui::Color32::from_rgb(0x1a, 0x1d, 0x26);
const CARD2: egui::Color32 = egui::Color32::from_rgb(0x22, 0x26, 0x31);
const BG: egui::Color32 = egui::Color32::from_rgb(0x12, 0x14, 0x1b);

/// How often the window asks for fresh figures while it is on screen.
const REFRESH: Duration = Duration::from_secs(2);
/// How long the search box has to be left alone before it is searched.
const TYPING_PAUSE: Duration = Duration::from_millis(250);
/// How long a message in the bottom bar stays before the bar goes back to
/// showing the library.
const NOTICE_FOR: Duration = Duration::from_secs(12);

/// The choices offered for the automatic full scan, in hours.
const AUTO_SCAN_CHOICES: [(u32, &str); 6] = [
    (0, "Off"),
    (6, "Every 6 hours"),
    (12, "Every 12 hours"),
    (24, "Every day"),
    (72, "Every 3 days"),
    (168, "Every week"),
];

pub fn run(cfg: Config, background: bool) -> Result<()> {
    // Linux has no tray to start in, so --background there is the window-less
    // monitor. It does not take the window's lock: opening the window while
    // it runs works, and the window simply leaves the watching to it.
    #[cfg(not(windows))]
    if background {
        crate::watch_service(cfg, true);
        return Ok(());
    }

    let data_dir = data_dir_of(&cfg);
    let _ = std::fs::create_dir_all(&data_dir);

    // One Lyricarr at a time. A second copy drops a flag the running one picks
    // up (the tray thread on Windows, the window itself on Linux), so clicking
    // the shortcut again raises the window you already have instead of
    // opening a rival that fights over the database.
    let lock = crate::os::exclusive_lock(&data_dir.join("lyricarr.lock"));
    if lock.is_none() {
        let _ = std::fs::write(crate::tray::show_flag_path(&cfg), "show");
        return Ok(());
    }

    let prefs = Prefs::load(&data_dir);
    if prefs.scan_on_start && !cfg.library.roots.is_empty() {
        let _ = crate::os::lyricarr().arg("scan").arg("--quiet").spawn();
    }

    // The folder monitor and the automatic full scan. Started here rather than
    // by the window, so they run from the first moment - including under
    // --background, where the window may never paint a frame at all.
    let monitor = watch::Monitor::start(
        prefs.monitor(&cfg),
        crate::logging::Log::open(Path::new(&cfg.run.log_file), true),
    );

    let shared = crate::tray::Shared::new(prefs.minimize_to_tray);
    let shared_for_exit = shared.clone();
    #[cfg(windows)]
    crate::tray::spawn(cfg.clone(), shared.clone(), monitor.clone());
    // No tray, so nothing to wait for at exit.
    #[cfg(not(windows))]
    shared.tray_gone.store(true, Ordering::Relaxed);

    let mut vp = egui::ViewportBuilder::default()
        .with_inner_size([1120.0, 740.0])
        .with_min_inner_size([820.0, 520.0])
        .with_title("Lyricarr")
        // eframe always shows the window for its first frame (see below), so
        // in the tray it is briefly on screen however this is set. Shown
        // without activation it at least no longer takes the keyboard away
        // from whatever you had opened first at sign-in.
        .with_active(!background);
    if let Some(icon) = window_icon() {
        vp = vp.with_icon(icon);
    }
    // NOTE: we do NOT try to start hidden with vp.with_visible(false).
    // eframe sets that flag itself, unconditionally, and then calls
    // set_visible(true) in post_rendering once the first frame has been
    // painted - so the builder flag is simply overwritten and the window
    // always appeared. App::update hides it a couple of frames in instead.

    let scale = prefs.ui_scale;
    let opts = eframe::NativeOptions {
        viewport: vp,
        persist_window: true,
        // Keep the remembered window size and column widths next to the
        // database rather than buried in %APPDATA%.
        persistence_path: Some(data_dir.join("window.ron")),
        ..Default::default()
    };
    let monitor_for_app = monitor.clone();
    let res = eframe::run_native(
        "Lyricarr",
        opts,
        Box::new(move |cc| {
            style(&cc.egui_ctx, scale);
            Ok(Box::new(App::new(
                cc.egui_ctx.clone(),
                cfg,
                prefs,
                shared,
                monitor_for_app,
                background,
            )))
        }),
    );
    // The window has finished. Ask the tray thread to take its icon out of
    // the notification area before the process ends - ending while it is
    // parked in its message pump drops no TrayIcon and leaves a dead icon
    // behind, which is how four of them accumulated.
    shared_for_exit.shutting_down.store(true, Ordering::Relaxed);
    let waited = std::time::Instant::now();
    while !shared_for_exit.tray_is_gone()
        && waited.elapsed() < std::time::Duration::from_millis(900)
    {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    monitor.shutdown();

    // Hold the single-instance lock until the window is really gone.
    drop(lock);
    res.map_err(|e| anyhow::anyhow!("{e}"))
}

fn data_dir_of(cfg: &Config) -> PathBuf {
    Path::new(&cfg.run.database)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(Config::here)
}

fn window_icon() -> Option<Arc<egui::IconData>> {
    let img = image::load_from_memory(include_bytes!("../assets/lyricarr64.png"))
        .ok()?
        .into_rgba8();
    let (width, height) = img.dimensions();
    Some(Arc::new(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }))
}

fn style(ctx: &egui::Context, zoom: f32) {
    // Always dark. egui 0.29 follows the Windows light/dark setting by default
    // and keeps a separate style for each, and set_visuals / set_style only
    // touch whichever is active at the time. This runs before the first frame,
    // when that is the dark one - so on a PC in light mode the first frame
    // swapped in egui's stock light style: white buttons on the dark panels,
    // dim grey text, and none of the font sizes below. A Windows theme that
    // changes with the time of day flipped the window while it was open.
    ctx.set_theme(egui::Theme::Dark);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = egui::Color32::from_rgb(0x0d, 0x0f, 0x14);
    v.faint_bg_color = egui::Color32::from_rgb(0x1b, 0x1e, 0x27); // table stripes
    v.widgets.inactive.bg_fill = CARD2;
    v.widgets.inactive.weak_bg_fill = CARD2;
    // A faint outline on everything at rest. Without one an unticked checkbox
    // or an empty text box is a slightly different dark grey on a dark grey
    // card - there, but only if you already know where to look.
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0f32, egui::Color32::from_rgb(0x3a, 0x41, 0x52));
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0f32, ACCENT.linear_multiply(0.7));
    v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x2c, 0x31, 0x3f);
    v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(0x2c, 0x31, 0x3f);
    v.widgets.active.bg_fill = ACCENT.linear_multiply(0.5);
    v.selection.bg_fill = ACCENT.linear_multiply(0.35);
    v.selection.stroke.color = INK;
    v.widgets.noninteractive.fg_stroke.color = INK;
    v.widgets.inactive.fg_stroke.color = INK;
    v.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
    v.widgets.active.fg_stroke.color = egui::Color32::WHITE;
    v.hyperlink_color = ACCENT;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
    ] {
        w.rounding = egui::Rounding::same(7.0);
    }
    v.window_rounding = egui::Rounding::same(10.0);

    let mut s = (*ctx.style_of(egui::Theme::Dark)).clone();
    s.visuals = v;
    s.spacing.item_spacing = egui::vec2(9.0, 8.0);
    s.spacing.button_padding = egui::vec2(12.0, 7.0);
    s.spacing.slider_width = 190.0;
    // Everything that does not ask for a size explicitly gets the body size,
    // not egui's 12.5pt default.
    use egui::{FontFamily, FontId, TextStyle};
    s.text_styles = [
        (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(BODY, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(BODY, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(SMALL, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();
    ctx.set_style_of(egui::Theme::Dark, s);
    ctx.set_zoom_factor(zoom);
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Tracks,
    Fix,
    Coverage,
    Activity,
    Settings,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Tracks, Tab::Fix, Tab::Coverage, Tab::Activity, Tab::Settings];
    fn label(self) -> &'static str {
        match self {
            Tab::Tracks => "  Tracks  ",
            Tab::Fix => "  Find by hand  ",
            Tab::Coverage => "  Coverage  ",
            Tab::Activity => "  Activity  ",
            Tab::Settings => "  Settings  ",
        }
    }
    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// The manual override: search LRCLIB by hand for a track the matcher gave up
/// on, look at what comes back, and pick one.
#[derive(Default)]
struct Fix {
    track: Option<Row>,
    artist: String,
    title: String,
    busy: Arc<AtomicBool>,
    results: Arc<Mutex<Option<Vec<Candidate>>>>,
    preview: Option<usize>,
    done: String,
}

/// What a folder picked in Settings is for.
#[derive(Clone, Copy)]
enum Pick {
    AddRoot,
    ScanOne,
}

/// The folder picker.
///
/// On Windows it runs right here: the system dialog keeps the window's
/// messages flowing while it is open. On Linux it goes through the desktop
/// portal, and rfd waits for the answer by blocking - the window then stops
/// answering the desktop, and GNOME offers to "Force Quit" it after five
/// seconds of someone browsing for a folder. So there it runs on a thread of
/// its own and the answer is picked up on a later frame.
#[derive(Default)]
struct Picker {
    busy: Arc<AtomicBool>,
    answer: Arc<Mutex<Option<Picked>>>,
}

/// What a picker was for, the folder chosen (if any), and how long it was
/// open - a "nothing" that came back at once means no picker could be shown.
type Picked = (Pick, Option<PathBuf>, Duration);

impl Picker {
    fn open(&self, ctx: &egui::Context, what: Pick, title: &'static str, start: Option<PathBuf>) {
        let dialog = move || {
            let mut d = rfd::FileDialog::new().set_title(title);
            if let Some(s) = start {
                d = d.set_directory(s);
            }
            let t = Instant::now();
            (d.pick_folder(), t.elapsed())
        };
        if cfg!(windows) {
            let (dir, took) = dialog();
            *self.answer.lock().unwrap() = Some((what, dir, took));
            return;
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let (busy, answer, ctx) = (self.busy.clone(), self.answer.clone(), ctx.clone());
        std::thread::spawn(move || {
            let (dir, took) = dialog();
            *answer.lock().unwrap() = Some((what, dir, took));
            busy.store(false, Ordering::SeqCst);
            ctx.request_repaint();
        });
    }

    fn take(&self) -> Option<Picked> {
        self.answer.lock().ok().and_then(|mut a| a.take())
    }
}

/// The engine settings the Settings tab can change, held as text while they are
/// being typed so a half-finished number never reaches `config.toml`.
struct Draft {
    rps: String,
    workers: String,
    timeout: String,
    plain: String,
    overwrite: bool,
    instrumental_marker: bool,
    min_chars: String,
    min_score: String,
    min_title_ratio: String,
    plain_min_title_ratio: String,
}

impl Draft {
    fn from(cfg: &Config) -> Self {
        Self {
            rps: cfg.lrclib.requests_per_second.to_string(),
            workers: cfg.lrclib.workers.to_string(),
            timeout: cfg.lrclib.timeout_seconds.to_string(),
            plain: cfg.output.plain_lyrics.clone(),
            overwrite: cfg.output.overwrite,
            instrumental_marker: cfg.output.instrumental_marker,
            min_chars: cfg.output.min_lyrics_chars.to_string(),
            min_score: cfg.lrclib.min_score.to_string(),
            min_title_ratio: cfg.lrclib.min_title_ratio.to_string(),
            plain_min_title_ratio: cfg.lrclib.plain_min_title_ratio.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// the loader: every database read, off the drawing thread
// ---------------------------------------------------------------------------

/// What the window wants from the database right now.
#[derive(Clone, PartialEq)]
struct Query {
    filter: String,
    needle: String,
    limit: usize,
    sort_col: usize,
    sort_desc: bool,
    /// Read the log tail as well - only while the Activity tab is open.
    log: bool,
    /// The artist coverage table, recounted whenever the number changes. Only
    /// asked for while the Coverage tab is open: it is a GROUP BY over every
    /// track, far too slow to repeat on a timer.
    coverage: Option<u64>,
}

impl Query {
    fn same_rows(&self, o: &Query) -> bool {
        self.filter == o.filter
            && self.needle == o.needle
            && self.limit == o.limit
            && self.sort_col == o.sort_col
            && self.sort_desc == o.sort_desc
    }
}

/// The scan-state rows of the meta table, read on every refresh.
#[derive(Clone, Default)]
struct Meta {
    /// A full scan's heartbeat is fresh.
    full_live: bool,
    /// A hook, monitor or one-folder run's heartbeat is fresh.
    quick_live: bool,
    /// The full scan's live figures, as the JSON it writes.
    progress: String,
    last_full_scan: Option<chrono::DateTime<chrono::Utc>>,
}

/// One answer from the loader.
struct Snapshot {
    db_ok: bool,
    counts: HashMap<String, i64>,
    rows: Arc<Vec<Row>>,
    meta: Meta,
    coverage: Option<Arc<Vec<tools::ArtistCoverage>>>,
    log: Option<Arc<Vec<String>>>,
}

#[derive(Default)]
struct LoaderState {
    db: Option<Store>,
    version: Option<i64>,
    answered: Option<Query>,
    counts: HashMap<String, i64>,
    rows: Arc<Vec<Row>>,
    coverage: Option<(u64, Arc<Vec<tools::ArtistCoverage>>)>,
    log_stamp: Option<(u64, Option<std::time::SystemTime>)>,
    log: Arc<Vec<String>>,
}

/// The window's half of the loader thread.
struct Loader {
    want: Arc<Mutex<Option<Query>>>,
    got: Arc<Mutex<Option<Snapshot>>>,
    wake: std::sync::mpsc::Sender<()>,
}

impl Loader {
    fn start(cfg: Config, ctx: egui::Context) -> Self {
        let want: Arc<Mutex<Option<Query>>> = Arc::new(Mutex::new(None));
        let got: Arc<Mutex<Option<Snapshot>>> = Arc::new(Mutex::new(None));
        let (wake, rx) = std::sync::mpsc::channel::<()>();
        {
            let (want, got) = (want.clone(), got.clone());
            let _ = std::thread::Builder::new().name("loader".into()).spawn(move || {
                let mut st = LoaderState::default();
                while rx.recv().is_ok() {
                    // Only the newest question matters.
                    while rx.try_recv().is_ok() {}
                    let Some(q) = want.lock().ok().and_then(|mut w| w.take()) else {
                        continue;
                    };
                    let snap = load(&mut st, &cfg, &q);
                    if let Ok(mut g) = got.lock() {
                        *g = Some(snap);
                    }
                    ctx.request_repaint();
                }
            });
        }
        Self { want, got, wake }
    }

    fn ask(&self, q: Query) {
        if let Ok(mut w) = self.want.lock() {
            *w = Some(q);
        }
        let _ = self.wake.send(());
    }

    fn take(&self) -> Option<Snapshot> {
        self.got.lock().ok().and_then(|mut g| g.take())
    }

    /// Wait for the first answer, so the window opens with its figures in
    /// place instead of drawing an empty frame and filling in a moment later.
    fn wait(&self, limit: Duration) -> Option<Snapshot> {
        let until = Instant::now() + limit;
        loop {
            if let Some(s) = self.take() {
                return Some(s);
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn load(st: &mut LoaderState, cfg: &Config, q: &Query) -> Snapshot {
    if st.db.is_none() {
        st.db = Store::open_readonly(Path::new(&cfg.run.database)).ok();
        st.version = None;
    }
    let mut meta = Meta::default();
    let db_ok = st.db.is_some();
    if let Some(db) = st.db.as_ref() {
        // The heavy part - the counts and the sorted, filtered rows - only
        // when another connection has committed something since last time, or
        // the question itself has changed. An idle library asks nothing.
        let version = db.data_version();
        let db_moved = version.is_none() || version != st.version;
        let same_question = st.answered.as_ref().map(|a| a.same_rows(q)).unwrap_or(false);
        // The counts do not depend on the search, so typing does not redo them.
        if db_moved {
            if let Ok(c) = db.counts() {
                st.counts = c;
            }
        }
        if db_moved || !same_question {
            let f = if q.filter == "all" { None } else { Some(q.filter.as_str()) };
            let sort = store::Sort {
                col: SortCol::ALL[q.sort_col.min(SortCol::ALL.len() - 1)],
                desc: q.sort_desc,
            };
            if let Ok(r) = db.browse_sorted(f, &q.needle, q.limit, sort) {
                st.rows = Arc::new(r);
            }
            st.version = version;
            st.answered = Some(q.clone());
        }
        if let Some(gen) = q.coverage {
            if st.coverage.as_ref().map(|(g, _)| *g != gen).unwrap_or(true) {
                let cov = tools::artist_coverage(db, 5, 300).unwrap_or_default();
                st.coverage = Some((gen, Arc::new(cov)));
            }
        }

        let when = |k: &str| db.get_meta(k).and_then(|s| store::parse_iso(&s));
        let fresh_within = |k: &str, secs: i64| {
            when(k)
                .map(|t| (chrono::Utc::now() - t).num_seconds() < secs)
                .unwrap_or(false)
        };
        // SCANNING when the heartbeat is fresh - see scan.rs
        meta.full_live = fresh_within("scan_heartbeat", 60);
        meta.quick_live = fresh_within("quick_heartbeat", 20);
        if meta.full_live {
            meta.progress = db.get_meta("scan_progress").unwrap_or_default();
        }
        meta.last_full_scan = when("last_full_scan").or_else(|| when("last_scan"));
    }

    // Reading 256 KB of log is wasted work unless the tab that shows it is
    // open, and even then only when the file has actually grown.
    let mut log = None;
    if q.log {
        let p = Path::new(&cfg.run.log_file);
        let stamp = std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()));
        if stamp != st.log_stamp {
            st.log = Arc::new(tail(p, 500));
            st.log_stamp = stamp;
        }
        log = Some(st.log.clone());
    }

    Snapshot {
        db_ok,
        counts: st.counts.clone(),
        rows: st.rows.clone(),
        meta,
        coverage: st.coverage.as_ref().map(|(_, c)| c.clone()),
        log,
    }
}

// ---------------------------------------------------------------------------
// the app
// ---------------------------------------------------------------------------

struct App {
    ctx: egui::Context,
    cfg: Config,
    data_dir: PathBuf,
    prefs: Prefs,
    /// Preferences changed by typing, written once the typing pauses.
    prefs_dirty: bool,
    shared: Arc<crate::tray::Shared>,
    monitor: Arc<watch::Monitor>,
    /// The monitor's state as of this frame.
    mon: watch::Status,
    loader: Loader,
    last_ask: Instant,
    ask_now: bool,
    /// When the search box last changed.
    typed_at: Option<Instant>,
    coverage_gen: u64,
    db_ok: bool,
    counts: HashMap<String, i64>,
    rows: Arc<Vec<Row>>,
    meta: Meta,
    /// The selected track, by path. An index into `rows` pointed at whatever
    /// row moved into that slot when a refresh reordered the list.
    sel: Option<String>,
    /// A right-click menu was open last frame. Fresh rows wait until it
    /// closes, so the menu cannot end up acting on a different track.
    menu_open: bool,
    tab: Tab,
    /// A full scan started from this window.
    child: Option<std::process::Child>,
    /// A "scan just this folder" run.
    side_child: Option<std::process::Child>,
    log_tail: Arc<Vec<String>>,
    notice: String,
    notice_seen: String,
    notice_at: Instant,
    copy_pending: Option<String>,
    fix: Fix,
    cov: Arc<Vec<tools::ArtistCoverage>>,
    strays: Arc<Mutex<Option<Vec<tools::Stray>>>>,
    stray_busy: Arc<AtomicBool>,
    stray_stop: Arc<AtomicBool>,
    /// Registering the scheduled task runs PowerShell, which takes seconds;
    /// it used to freeze the window for all of them.
    task_busy: Arc<AtomicBool>,
    task_result: Arc<Mutex<Option<String>>>,
    draft: Draft,
    start_with_windows: bool,
    viewer: Option<(PathBuf, String)>,
    confirm_rescan: bool,
    /// The "type or paste a folder path" box in Settings.
    add_path: String,
    picker: Picker,
    /// Frames to wait before hiding the window. Hiding cannot be done in the
    /// same frame that asks for it: at startup eframe re-shows the window after
    /// the first frame, and after a Minimized(false) winit re-issues SW_SHOW.
    /// Both would undo a hide done inline, which is exactly what used to
    /// happen to --background and to minimise-to-tray.
    hide_in: u32,
}

impl App {
    fn new(
        ctx: egui::Context,
        cfg: Config,
        prefs: Prefs,
        shared: Arc<crate::tray::Shared>,
        monitor: Arc<watch::Monitor>,
        background: bool,
    ) -> Self {
        let tab = Tab::ALL.get(prefs.tab).copied().unwrap_or(Tab::Tracks);
        let loader = Loader::start(cfg.clone(), ctx.clone());
        let mut me = Self {
            ctx,
            data_dir: data_dir_of(&cfg),
            draft: Draft::from(&cfg),
            cfg,
            prefs,
            prefs_dirty: false,
            shared,
            mon: monitor.status(),
            monitor,
            loader,
            last_ask: Instant::now(),
            ask_now: false,
            typed_at: None,
            coverage_gen: 1,
            db_ok: false,
            counts: Default::default(),
            rows: Arc::new(Vec::new()),
            meta: Meta::default(),
            sel: None,
            menu_open: false,
            tab,
            child: None,
            side_child: None,
            log_tail: Arc::new(Vec::new()),
            notice: String::new(),
            notice_seen: String::new(),
            notice_at: Instant::now(),
            copy_pending: None,
            fix: Fix::default(),
            cov: Arc::new(Vec::new()),
            strays: Arc::new(Mutex::new(None)),
            stray_busy: Arc::new(AtomicBool::new(false)),
            stray_stop: Arc::new(AtomicBool::new(false)),
            task_busy: Arc::new(AtomicBool::new(false)),
            task_result: Arc::new(Mutex::new(None)),
            start_with_windows: crate::os::is_start_at_login(),
            viewer: None,
            confirm_rescan: false,
            add_path: String::new(),
            picker: Picker::default(),
            hide_in: if background { 3 } else { 0 },
        };
        me.loader.ask(me.query());
        if let Some(s) = me.loader.wait(Duration::from_millis(1500)) {
            me.apply(s);
        }
        me
    }

    fn save_prefs(&mut self) {
        self.prefs.save(&self.data_dir);
        self.prefs_dirty = false;
    }

    /// Tell the monitor about a change to the preferences or the folders.
    fn reconfigure_monitor(&self) {
        self.monitor.configure(self.prefs.monitor(&self.cfg));
    }

    fn cfg_path(&self) -> PathBuf {
        Config::default_config_path()
    }

    fn query(&self) -> Query {
        Query {
            filter: self.prefs.filter.clone(),
            needle: self.prefs.needle.clone(),
            limit: self.prefs.row_limit,
            sort_col: self.prefs.sort_col,
            sort_desc: self.prefs.sort_desc,
            log: self.tab == Tab::Activity,
            coverage: (self.tab == Tab::Coverage).then_some(self.coverage_gen),
        }
    }

    /// Ask for fresh figures on the next frame.
    fn refresh(&mut self) {
        self.ask_now = true;
    }

    fn apply(&mut self, s: Snapshot) {
        self.db_ok = s.db_ok;
        self.counts = s.counts;
        self.rows = s.rows;
        self.meta = s.meta;
        if let Some(c) = s.coverage {
            self.cov = c;
        }
        if let Some(l) = s.log {
            self.log_tail = l;
        }
    }

    /// Once a frame: ask the loader when it is time, take its answer when
    /// there is one, and notice scans this window started having finished.
    fn pump(&mut self) {
        let typing = self.typed_at.map(|t| t.elapsed() < TYPING_PAUSE).unwrap_or(false);
        if typing {
            // Come back when the pause is up, or the search would wait for
            // the next mouse move to run.
            self.ctx.request_repaint_after(TYPING_PAUSE);
        } else {
            if self.typed_at.take().is_some() {
                self.ask_now = true;
            }
            if self.prefs_dirty {
                self.save_prefs();
            }
            if self.ask_now || self.last_ask.elapsed() >= REFRESH {
                self.ask_now = false;
                self.last_ask = Instant::now();
                self.loader.ask(self.query());
                // A second copy asks for this window by leaving a flag. The
                // tray thread answers it on Windows; with no tray, the window
                // does.
                #[cfg(not(windows))]
                {
                    let flag = crate::tray::show_flag_path(&self.cfg);
                    if flag.exists() {
                        let _ = std::fs::remove_file(&flag);
                        self.ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
            }
        }
        // Read and cleared once a frame: the Tracks tab sets it again while its
        // menu is still open. Left set, a menu open when the tab was switched
        // would have stopped every refresh until you went back to Tracks.
        if !std::mem::take(&mut self.menu_open) {
            if let Some(s) = self.loader.take() {
                self.apply(s);
            }
        }
        for c in [&mut self.child, &mut self.side_child] {
            if c.as_mut().map(|p| !matches!(p.try_wait(), Ok(None))).unwrap_or(false) {
                *c = None;
                self.ask_now = true;
            }
        }
        self.mon = self.monitor.status();

        if let Some(r) = self.task_result.lock().ok().and_then(|mut g| g.take()) {
            self.notice = r;
        }
        match self.picker.take() {
            Some((Pick::AddRoot, Some(dir), _)) => {
                self.add_root(dir);
            }
            Some((Pick::ScanOne, Some(dir), _)) => self.scan_folder(&dir),
            // Nothing chosen, and too quickly for anyone to have looked: no
            // picker could be shown at all (on Linux, no desktop portal).
            Some((_, None, took)) if took < Duration::from_millis(300) => {
                self.notice = "no folder picker is available here - type or paste the path \
                               into the box below instead"
                    .into();
            }
            _ => {}
        }
        // Messages in the bottom bar fade back to the library line on their
        // own, instead of the last one sitting there for the rest of the day.
        if self.notice != self.notice_seen {
            self.notice_seen = self.notice.clone();
            self.notice_at = Instant::now();
        } else if !self.notice.is_empty() && self.notice_at.elapsed() > NOTICE_FOR {
            self.notice.clear();
            self.notice_seen.clear();
        }
    }

    fn total(&self) -> i64 {
        self.counts.values().sum()
    }

    fn g(&self, k: &str) -> i64 {
        self.counts.get(k).copied().unwrap_or(0)
    }

    /// A full scan is running - this window's own, or anyone else's.
    fn full_live(&self) -> bool {
        self.meta.full_live || self.child.is_some()
    }

    /// New music is being looked up right now - by the monitor, the Lidarr
    /// hook, or a one-folder scan.
    fn quick_live(&self) -> bool {
        self.meta.quick_live || self.side_child.is_some() || self.mon.checking > 0
    }

    /// The one-line "what is it doing right now", read from the row the
    /// scanner refreshes on every heartbeat.
    fn live_line(&self) -> Option<String> {
        if self.meta.progress.trim().is_empty() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&self.meta.progress).ok()?;
        let n = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let secs = n("elapsed") as i64;
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        Some(format!(
            "walked {} files  -  {} looked up at {:.1}/s  -  {} synced, {} plain, {} instrumental, {} no match, {} errors  -  running {}h {:02}m",
            fmt(n("seen") as i64),
            fmt(n("lookups") as i64),
            n("rate"),
            fmt(n("synced") as i64),
            fmt(n("plain") as i64),
            fmt(n("instrumental") as i64),
            fmt(n("notfound") as i64),
            fmt(n("error") as i64),
            h,
            m,
        ))
    }

    /// The line under the coverage bar. Always exactly one line, whatever is
    /// happening, so that a scan starting or stopping no longer shoves the
    /// whole window down and back up.
    fn status_line(&self) -> (String, egui::Color32) {
        if self.full_live() {
            let line = self
                .live_line()
                .unwrap_or_else(|| "scan starting - walking the library...".into());
            return (line, OK);
        }
        let mut parts: Vec<String> = Vec::new();
        parts.push(match self.meta.last_full_scan {
            Some(t) => format!("idle - last full scan {}", ago(t)),
            None => "idle - no full scan yet".into(),
        });
        if self.mon.auto_scan_hours > 0 {
            if let Some(next) = self.mon.next_full_scan {
                let secs = (next - chrono::Utc::now()).num_seconds();
                parts.push(if secs <= 0 {
                    "automatic scan due now".into()
                } else {
                    format!("next automatic scan in {}", span(secs))
                });
            }
        }
        if self.mon.passive {
            parts.push("watched by the background service".into());
        } else if self.mon.watch && self.mon.watching() > 0 {
            parts.push(format!(
                "watching {} for new music",
                plural(self.mon.watching(), "folder", "folders")
            ));
        }
        (parts.join("   -   "), MUTED)
    }

    fn start_scan(&mut self, retry_now: bool) {
        // A scan with no folders exits at once; say what is missing instead
        // of "scan started" and then nothing at all.
        if self.cfg.library.roots.is_empty() {
            self.notice = "add a music folder first - Settings, Music folders".into();
            self.go_to(Tab::Settings);
            return;
        }
        let mut c = crate::os::lyricarr();
        c.arg("scan").arg("--quiet");
        if retry_now {
            c.arg("--retry-now");
        }
        match c.spawn() {
            Ok(c) => {
                self.child = Some(c);
                self.notice = if retry_now {
                    "full rescan started - every track without lyrics will be asked about again"
                        .into()
                } else {
                    "scan started".into()
                };
            }
            Err(e) => self.notice = format!("could not start the scan: {e}"),
        }
    }

    fn stop_scan(&mut self) {
        match std::fs::write(crate::scan::stop_flag_path(&self.cfg), "stop") {
            Ok(()) => {
                self.notice = "stopping - it will finish the tracks already in flight".into()
            }
            Err(e) => self.notice = format!("could not ask it to stop: {e}"),
        }
    }

    fn retry(&mut self, status: &str) {
        match Store::open(Path::new(&self.cfg.run.database)) {
            Ok(w) => match w.reset_status(status) {
                Ok(n) => {
                    self.notice =
                        format!("{n} {status} tracks will be asked about again on the next scan");
                    self.refresh();
                }
                Err(e) => self.notice = format!("could not clear retry windows: {e}"),
            },
            Err(e) => self.notice = format!("could not open the database for writing: {e}"),
        }
    }

    /// Hand a track to the Find-by-hand tab, pre-filled from its tags.
    fn open_fix(&mut self, row: &Row) {
        self.fix = Fix {
            artist: row.artist.clone(),
            title: row.title.clone(),
            track: Some(row.clone()),
            ..Default::default()
        };
        self.tab = Tab::Fix;
        self.prefs.tab = Tab::Fix.index();
        self.save_prefs();
    }

    fn search_lrclib(&mut self) {
        if self.fix.busy.load(Ordering::Relaxed) {
            return;
        }
        let (artist, title) = (self.fix.artist.clone(), self.fix.title.clone());
        if title.trim().is_empty() {
            self.notice = "a title is the one thing LRCLIB cannot search without".into();
            return;
        }
        let cfg = self.cfg.clone();
        let busy = self.fix.busy.clone();
        let out = self.fix.results.clone();
        let ctx = self.ctx.clone();
        busy.store(true, Ordering::Relaxed);
        *out.lock().unwrap() = None;
        std::thread::spawn(move || {
            let r = tools::manual_search(&cfg, &artist, &title);
            *out.lock().unwrap() = Some(r);
            busy.store(false, Ordering::Relaxed);
            // Show the answer now, not on the next timed repaint.
            ctx.request_repaint();
        });
    }

    fn find_strays(&mut self) {
        if self.stray_busy.load(Ordering::Relaxed) {
            self.stray_stop.store(true, Ordering::Relaxed);
            return;
        }
        let cfg = self.cfg.clone();
        let busy = self.stray_busy.clone();
        let stop = self.stray_stop.clone();
        let out = self.strays.clone();
        let ctx = self.ctx.clone();
        stop.store(false, Ordering::Relaxed);
        busy.store(true, Ordering::Relaxed);
        *out.lock().unwrap() = None;
        std::thread::spawn(move || {
            let r = tools::find_strays(&cfg, 2000, &stop);
            *out.lock().unwrap() = Some(r);
            busy.store(false, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }

    /// Write the Settings tab's engine values into `config.toml` line by line,
    /// leaving its comments intact, then re-read it so the window and the next
    /// scan agree about what is set.
    fn save_engine_settings(&mut self) {
        let p = self.cfg_path();
        if let Err(e) = Config::ensure_file(&p) {
            self.notice = format!("could not create config.toml: {e:#}");
            return;
        }
        let mut wrote = 0usize;
        let mut problems: Vec<String> = Vec::new();
        {
            let mut set = |section: &str, key: &str, value: String| {
                match crate::config_edit::set_value(&p, section, key, &value) {
                    Ok(true) => wrote += 1,
                    Ok(false) => problems.push(format!("{section}.{key} is not in config.toml")),
                    Err(e) => problems.push(format!("{section}.{key}: {e}")),
                }
            };
            let num = |s: &str, fallback: f64| s.trim().parse::<f64>().unwrap_or(fallback);

            set(
                "lrclib",
                "requests_per_second",
                num(&self.draft.rps, self.cfg.lrclib.requests_per_second)
                    .clamp(0.2, 20.0)
                    .to_string(),
            );
            set(
                "lrclib",
                "workers",
                (num(&self.draft.workers, self.cfg.lrclib.workers as f64).round() as i64)
                    .clamp(1, 16)
                    .to_string(),
            );
            set(
                "lrclib",
                "timeout_seconds",
                num(&self.draft.timeout, self.cfg.lrclib.timeout_seconds)
                    .clamp(3.0, 120.0)
                    .to_string(),
            );
            set(
                "lrclib",
                "min_score",
                num(&self.draft.min_score, self.cfg.lrclib.min_score)
                    .clamp(0.0, 1.0)
                    .to_string(),
            );
            set(
                "lrclib",
                "min_title_ratio",
                num(&self.draft.min_title_ratio, self.cfg.lrclib.min_title_ratio)
                    .clamp(0.0, 1.0)
                    .to_string(),
            );
            set(
                "lrclib",
                "plain_min_title_ratio",
                num(
                    &self.draft.plain_min_title_ratio,
                    self.cfg.lrclib.plain_min_title_ratio,
                )
                .clamp(0.0, 1.0)
                .to_string(),
            );
            let plain = match self.draft.plain.as_str() {
                "txt" => "txt",
                "skip" => "skip",
                _ => "lrc",
            };
            set("output", "plain_lyrics", format!("\"{plain}\""));
            set("output", "overwrite", self.draft.overwrite.to_string());
            set(
                "output",
                "instrumental_marker",
                self.draft.instrumental_marker.to_string(),
            );
            set(
                "output",
                "min_lyrics_chars",
                (num(&self.draft.min_chars, self.cfg.output.min_lyrics_chars as f64).round()
                    as i64)
                    .clamp(0, 10_000)
                    .to_string(),
            );
        }

        match Config::load(Some(&p)) {
            Ok(c) => {
                self.cfg = c;
                self.draft = Draft::from(&self.cfg);
                self.reconfigure_monitor();
                self.notice = if problems.is_empty() {
                    format!("saved {wrote} settings to config.toml")
                } else {
                    format!("saved {wrote}; {}", problems.join("; "))
                };
            }
            Err(e) => self.notice = format!("saved, but config.toml no longer parses: {e:#}"),
        }
    }

    /// Write `library.roots` back into config.toml and re-read it.
    fn save_roots(&mut self, roots: Vec<String>) -> bool {
        if let Err(e) = Config::ensure_file(&self.cfg_path()) {
            self.notice = format!("could not create config.toml: {e:#}");
            return false;
        }
        let value = crate::config_edit::toml_path_array(&roots);
        match crate::config_edit::set_value(&self.cfg_path(), "library", "roots", &value) {
            Ok(true) => match Config::load(Some(&self.cfg_path())) {
                Ok(c) => {
                    self.cfg = c;
                    // Start watching the new list straight away, and let the
                    // schedule see that there is (or is no longer) anything
                    // for a full scan to do.
                    self.reconfigure_monitor();
                    self.monitor.check_now();
                    self.notice = format!("music folders saved ({})", roots.len());
                    return true;
                }
                Err(e) => self.notice = format!("saved, but config.toml no longer parses: {e:#}"),
            },
            Ok(false) => {
                self.notice = "could not find roots = ... in the [library] section".into()
            }
            Err(e) => self.notice = format!("could not write config.toml: {e}"),
        }
        false
    }

    /// Scan one folder now. A targeted run, so it is exempt from the
    /// one-full-scan-at-a-time lock and works during the weekly pass.
    fn scan_folder(&mut self, dir: &Path) {
        match crate::os::lyricarr()
            .arg("scan")
            .arg("--quiet")
            .arg("--path")
            .arg(dir)
            .spawn()
        {
            Ok(c) => {
                self.side_child = Some(c);
                self.notice = format!("scanning {}", dir.display());
            }
            Err(e) => self.notice = format!("could not start that scan: {e}"),
        }
    }

    /// Register or remove the scheduled task, off the drawing thread.
    fn install_task(&mut self, remove: bool) {
        if self.task_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let (busy, out, ctx) = (self.task_busy.clone(), self.task_result.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let mut c = crate::os::lyricarr();
            c.arg("install-task");
            if remove {
                c.arg("--remove");
            }
            let msg = match c.output() {
                Ok(o) => {
                    let txt = String::from_utf8_lossy(&o.stdout);
                    let err = String::from_utf8_lossy(&o.stderr);
                    if o.status.success() {
                        txt.lines()
                            .filter(|l| !l.trim().is_empty())
                            .last()
                            .unwrap_or("done")
                            .to_string()
                    } else {
                        format!("scheduled task failed: {}", err.trim())
                    }
                }
                Err(e) => format!("could not run install-task: {e}"),
            };
            if let Ok(mut g) = out.lock() {
                *g = Some(msg);
            }
            busy.store(false, Ordering::SeqCst);
            ctx.request_repaint();
        });
    }

    /// Roughly how long re-asking LRCLIB about every miss will take, for the
    /// full rescan's confirmation. A miss costs three or four requests - the
    /// whole ladder of attempts - against the configured rate.
    fn rescan_estimate(&self) -> (i64, String) {
        let n = self.g(store::ST_NOTFOUND) + self.g(store::ST_ERROR);
        let rps = self.cfg.lrclib.requests_per_second.max(0.1);
        let secs = (n as f64 * 3.5 / rps) as i64;
        (n, span(secs.max(60)))
    }
}

fn tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(data) = std::fs::read(path) else { return Vec::new() };
    let start = data.len().saturating_sub(256 * 1024);
    let text = String::from_utf8_lossy(&data[start..]);
    let all: Vec<&str> = text.lines().collect();
    all.iter().rev().take(lines).rev().map(|s| s.to_string()).collect()
}

/// A headline figure. Every card is the same width, worked out from the
/// window, so a count gaining a digit no longer nudges the cards beside it.
fn card(ui: &mut egui::Ui, width: f32, label: &str, value: String, colour: egui::Color32) {
    egui::Frame::none()
        .fill(CARD)
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::symmetric(14.0, 10.0))
        .show(ui, |ui| {
            ui.set_width(width - 28.0);
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(label).size(SMALL).color(MUTED))
                        .truncate(),
                );
                ui.label(egui::RichText::new(value).size(24.0).strong().color(colour));
            });
        });
}

/// The little triangle beside the sorted column, painted rather than typed.
/// The "small triangle" characters (U+25B4 / U+25BE) are not in egui's bundled
/// fonts and came out as an empty box.
fn sort_arrow(ui: &egui::Ui, after: egui::Rect, desc: bool) {
    let r = 4.0;
    let c = egui::pos2(after.right() + r + 3.0, after.center().y);
    let pts = if desc {
        vec![
            egui::pos2(c.x - r, c.y - r * 0.55),
            egui::pos2(c.x + r, c.y - r * 0.55),
            egui::pos2(c.x, c.y + r * 0.75),
        ]
    } else {
        vec![
            egui::pos2(c.x - r, c.y + r * 0.55),
            egui::pos2(c.x + r, c.y + r * 0.55),
            egui::pos2(c.x, c.y - r * 0.75),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(pts, ACCENT, egui::Stroke::NONE));
}

fn status_colour(s: &str) -> egui::Color32 {
    match s {
        store::ST_SYNCED => OK,
        store::ST_PLAIN => ACCENT,
        store::ST_INSTRUMENTAL => MUTED,
        store::ST_NOTFOUND => WARN,
        store::ST_ERROR => BAD,
        _ => MUTED,
    }
}

/// A status as a filled pill, which reads at a glance in a long list far
/// better than coloured words do.
fn pill(ui: &mut egui::Ui, text: &str, colour: egui::Color32) -> egui::Response {
    egui::Frame::none()
        .fill(colour.linear_multiply(0.22))
        .rounding(egui::Rounding::same(999.0))
        .inner_margin(egui::Margin::symmetric(8.0, 2.0))
        .show(ui, |ui| {
            cell(ui, egui::RichText::new(text).size(SMALL).strong().color(colour));
        })
        .response
}

/// Text in a table cell. Not selectable: egui 0.29 makes every label
/// selectable by default, and a selectable label senses clicks itself - so
/// clicking on a track's title, the most natural place to click, went to the
/// text instead of the row. Neither selecting it nor right-clicking it for the
/// menu worked; only the blank space beside the words did.
fn cell(ui: &mut egui::Ui, text: egui::RichText) {
    ui.add(egui::Label::new(text).selectable(false));
}

/// A pill that can be clicked. Separate from `pill`, because the ones in the
/// track table must not swallow the clicks meant for their row.
fn pill_button(ui: &mut egui::Ui, id: &str, text: &str, colour: egui::Color32) -> egui::Response {
    let r = pill(ui, text, colour);
    let r = ui.interact(r.rect, ui.id().with(id), egui::Sense::click());
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        if let Some(s) = self.copy_pending.take() {
            ctx.output_mut(|o| o.copied_text = s);
        }

        // A hide that was asked for a frame or two ago - see `hide_in`.
        if self.hide_in > 0 {
            self.hide_in -= 1;
            if self.hide_in == 0 {
                crate::os::hide_window();
            } else {
                ctx.request_repaint();
            }
        }

        // The tray's Quit asks the window to go away cleanly.
        if self.shared.quit_requested.load(Ordering::SeqCst) {
            self.save_prefs();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Close and minimise, per the Settings tab.
        if ctx.input(|i| i.viewport().close_requested()) {
            if crate::os::HAS_TRAY && self.prefs.close_to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                crate::os::hide_window();
                self.notice =
                    "still running in the tray - click the tray icon to bring it back".into();
            }
            self.save_prefs();
        }
        // Minimise-to-tray is NOT handled here. It used to be, and it never
        // fired: the branch needed a frame where viewport().minimized was
        // Some(true), and a minimised window gets no WM_PAINT, so eframe never
        // painted one. The tray thread watches IsIconic instead - see
        // tray::run.

        self.pump();
        // No point waking up once a second for a window nobody can see; the
        // tray thread and the monitor carry on while we are hidden.
        if !crate::os::window_hidden() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        self.top_bar(ctx);
        self.bottom_bar(ctx);
        self.lrc_viewer(ctx);
        self.rescan_dialog(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin::symmetric(16.0, 14.0)))
            .show(ctx, |ui| {
                self.summary(ui);
                ui.add_space(10.0);
                // Explicit ids. Everything here was on egui's positional
                // auto-ids, which are only unique as long as the layout above
                // them never changes - and this layout changes every time a
                // tab is switched.
                let mut picked: Option<Tab> = None;
                ui.push_id("tabrow", |ui| {
                    ui.horizontal(|ui| {
                        for t in Tab::ALL {
                            let r = ui.push_id(t.index(), |ui| {
                                ui.selectable_label(self.tab == t, t.label())
                            });
                            if r.inner.clicked() {
                                picked = Some(t);
                            }
                        }
                    });
                });
                if let Some(t) = picked {
                    self.go_to(t);
                }
                ui.add_space(8.0);
                match self.tab {
                    Tab::Tracks => self.tracks_tab(ui),
                    Tab::Fix => self.fix_tab(ui),
                    Tab::Coverage => self.coverage_tab(ui),
                    Tab::Activity => self.activity_tab(ui),
                    Tab::Settings => self.settings_tab(ui),
                }
            });
    }
}

// ---------------------------------------------------------------------------
// chrome
// ---------------------------------------------------------------------------

impl App {
    fn go_to(&mut self, t: Tab) {
        if self.tab == t {
            return;
        }
        self.tab = t;
        self.prefs.tab = t.index();
        self.save_prefs();
        self.refresh();
    }

    /// The monitor's pill: what it is, what colour, and what hovering says.
    fn monitor_pill(&self) -> (String, egui::Color32, String) {
        let m = &self.mon;
        if self.quick_live() {
            let text = if m.checking > 0 {
                format!("CHECKING {} NEW", m.checking)
            } else {
                "CHECKING NEW MUSIC".into()
            };
            return (text, OK, "Looking up music that has just arrived.".into());
        }
        if m.passive {
            return (
                "WATCHED IN BACKGROUND".into(),
                ACCENT,
                "Another Lyricarr - the background service - is watching the music folders \
                 and running the automatic scans. This window leaves it to that one, and takes \
                 over if it stops."
                    .into(),
            );
        }
        if !m.watch {
            return (
                "NOT WATCHING".into(),
                MUTED,
                "Live monitoring is off - new music waits for the next scan. \
                 Click to turn it on."
                    .into(),
            );
        }
        if m.folders.is_empty() {
            return (
                "NO FOLDERS".into(),
                WARN,
                "Add a music folder in Settings and it will be watched.".into(),
            );
        }
        if m.waiting > 0 {
            return (
                format!("{} ARRIVING", m.waiting),
                ACCENT,
                "New files are waiting until they stop changing; then they are looked up.".into(),
            );
        }
        if m.partial() > 0 {
            return (
                "PARTLY WATCHED".into(),
                WARN,
                "Linux ran out of inotify watches, so some folders are not watched - new music \
                 there waits for the next full scan. Settings says how to raise the limit."
                    .into(),
            );
        }
        let off = m.unreachable();
        if off > 0 {
            return (
                format!("{} OFFLINE", plural(off, "FOLDER", "FOLDERS")),
                WARN,
                "A music folder is not reachable right now. Lyricarr keeps trying, and \
                 checks it over once it is back."
                    .into(),
            );
        }
        (
            "WATCHING".into(),
            ACCENT,
            format!(
                "Watching {} - new music gets its lyrics as it lands. Click for settings.",
                plural(m.folders.len(), "folder", "folders")
            ),
        )
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        let full = self.full_live();
        let (mtext, mcolour, mtip) = self.monitor_pill();
        egui::TopBottomPanel::top("head")
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(0x16, 0x19, 0x21))
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Lyricarr").size(23.0).strong().color(ACCENT));
                    ui.label(
                        egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .size(SMALL)
                            .color(MUTED),
                    );
                    ui.add_space(12.0);
                    if full {
                        pill(ui, "SCANNING", OK);
                    } else {
                        pill(ui, "IDLE", MUTED);
                    }
                    if pill_button(ui, "monitor-pill", &mtext, mcolour)
                        .on_hover_text(mtip)
                        .clicked()
                    {
                        self.go_to(Tab::Settings);
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Start and Stop trade places only for a full scan.
                        // The monitor's own small runs come and go every few
                        // minutes and show up in the pill instead - flipping
                        // this button for each of them would have it flicker.
                        if full {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Stop scan")
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(BAD.linear_multiply(0.75)),
                                )
                                .clicked()
                            {
                                self.stop_scan();
                            }
                        } else if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Scan now")
                                        .color(egui::Color32::BLACK)
                                        .strong(),
                                )
                                .fill(ACCENT),
                            )
                            .on_hover_text(
                                "Walks the library and looks up anything new or due. \
                                 Tracks that already have lyrics cost nothing.",
                            )
                            .clicked()
                        {
                            self.start_scan(false);
                        }
                        if ui
                            .add_enabled(!full, egui::Button::new("Full rescan..."))
                            .on_hover_text(
                                "Walks everything again and re-asks LRCLIB about every track \
                                 that has no lyrics yet, without waiting out the retry time",
                            )
                            .on_disabled_hover_text("a scan is already running")
                            .clicked()
                        {
                            self.confirm_rescan = true;
                        }
                        if crate::os::HAS_TRAY
                            && ui
                                .button("Hide to tray")
                                .on_hover_text("Keeps running - click the tray icon to come back")
                                .clicked()
                        {
                            self.save_prefs();
                            crate::os::hide_window();
                        }
                    });
                });
            });
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("foot")
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(0x10, 0x12, 0x18))
                    .inner_margin(egui::Margin::symmetric(16.0, 7.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let has_notice = !self.notice.is_empty();
                    if has_notice {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("clear").clicked() {
                                self.notice.clear();
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&self.notice).size(SMALL).color(INK),
                                    )
                                    .truncate(),
                                );
                            });
                        });
                    } else {
                        let text = if self.cfg.library.roots.is_empty() {
                            "no music folders yet - add one in Settings".to_string()
                        } else {
                            format!("library: {}", self.cfg.library.roots.join("  |  "))
                        };
                        ui.add(
                            egui::Label::new(egui::RichText::new(text).size(SMALL).color(MUTED))
                                .truncate(),
                        );
                    }
                });
            });
    }

    fn summary(&mut self, ui: &mut egui::Ui) {
        let total = self.total();
        let gap = ui.spacing().item_spacing.x;
        let w = ((ui.available_width() - gap * 5.0) / 6.0).floor().clamp(116.0, 220.0);
        ui.horizontal_wrapped(|ui| {
            card(ui, w, "TRACKS", fmt(total), INK);
            card(ui, w, "SYNCED", fmt(self.g(store::ST_SYNCED)), OK);
            card(ui, w, "PLAIN", fmt(self.g(store::ST_PLAIN)), ACCENT);
            card(ui, w, "INSTRUMENTAL", fmt(self.g(store::ST_INSTRUMENTAL)), MUTED);
            card(ui, w, "NO MATCH", fmt(self.g(store::ST_NOTFOUND)), WARN);
            card(ui, w, "ERRORS", fmt(self.g(store::ST_ERROR)), BAD);
        });
        ui.add_space(6.0);
        let done =
            self.g(store::ST_SYNCED) + self.g(store::ST_PLAIN) + self.g(store::ST_INSTRUMENTAL);
        let pct = if total > 0 { done as f32 / total as f32 } else { 0.0 };
        // The bar is always drawn, empty when there is nothing yet, so the
        // layout below it is the same from the first run on.
        let text = if total > 0 {
            format!(
                "{} of {} have lyrics or are known instrumental  -  {:.1}%",
                fmt(done),
                fmt(total),
                pct * 100.0
            )
        } else if self.cfg.library.roots.is_empty() {
            "nothing recorded yet - add your music folder in Settings to begin".into()
        } else {
            "nothing recorded yet - press Scan now".into()
        };
        ui.add(
            egui::ProgressBar::new(pct)
                .desired_height(18.0)
                .fill(ACCENT)
                .text(egui::RichText::new(text).size(SMALL).strong().color(INK)),
        );

        // What is happening this second. The headline counts move too slowly
        // to look like anything is happening at all.
        ui.add_space(4.0);
        let (line, colour) = self.status_line();
        let r = ui.add(
            egui::Label::new(egui::RichText::new(&line).size(SMALL).color(colour)).truncate(),
        );
        r.on_hover_text(line);
    }

    /// Asked before a full rescan, because it can run for many hours.
    fn rescan_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_rescan {
            return;
        }
        let (n, est) = self.rescan_estimate();
        let mut open = true;
        let mut go = false;
        let mut cancel = false;
        egui::Window::new("Full rescan")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(
                    "Walk every music folder again and ask LRCLIB once more about every track \
                     that has no lyrics yet - including the ones still waiting out their retry \
                     time.",
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!(
                        "That is {} tracks, which takes roughly {} at {} requests a second.",
                        fmt(n),
                        est,
                        self.cfg.lrclib.requests_per_second
                    ))
                    .color(WARN),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Tracks that already have lyrics are skipped, and you can stop it at any \
                         time - the next scan carries on from where it got to.",
                    )
                    .size(SMALL)
                    .color(MUTED),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Start full rescan")
                                    .color(egui::Color32::BLACK)
                                    .strong(),
                            )
                            .fill(ACCENT),
                        )
                        .clicked()
                    {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if go {
            self.start_scan(true);
        }
        if go || cancel || !open {
            self.confirm_rescan = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Tracks
// ---------------------------------------------------------------------------

const COLS: [(&str, SortCol, f32); 7] = [
    ("STATUS", SortCol::Status, 98.0),
    ("ARTIST", SortCol::Artist, 170.0),
    ("ALBUM", SortCol::Album, 170.0),
    ("TITLE", SortCol::Title, 250.0),
    ("TRIES", SortCol::Attempts, 62.0),
    ("RETRY DUE", SortCol::Retry, 92.0),
    ("LAST CHECKED", SortCol::Checked, 130.0),
];

#[derive(Clone, Copy)]
enum RowAction {
    Select,
    Fix,
    View,
    Folder,
    Copy,
    Instrumental,
    Forget,
}

impl App {
    fn tracks_tab(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("statusfilter")
                .selected_text(self.prefs.filter.clone())
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut self.prefs.filter, "all".into(), "all")
                        .changed();
                    for s in STATUSES {
                        changed |= ui
                            .selectable_value(&mut self.prefs.filter, s.to_string(), *s)
                            .changed();
                    }
                });
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.prefs.needle)
                    .hint_text("search artist, album, title or path")
                    .desired_width(300.0),
            );
            if r.changed() {
                // Searched once typing pauses, not on every key.
                self.typed_at = Some(Instant::now());
                self.prefs_dirty = true;
            }
            if ui.button("Clear").clicked() && !self.prefs.needle.is_empty() {
                self.prefs.needle.clear();
                changed = true;
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} rows (limit {})",
                    fmt(self.rows.len() as i64),
                    fmt(self.prefs.row_limit as i64)
                ))
                .size(SMALL)
                .color(MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Only as much of this as there is room for. A
                // right-to-left layout does not reserve space, so at a
                // narrower window the full sentence was painted straight
                // over the row count on the left.
                let room = ui.available_width();
                let hint = if room > 470.0 {
                    "drag a column edge to resize  -  click a header to sort  -  \
                     right-click a row for what to do with it"
                } else if room > 210.0 {
                    "right-click a row for actions"
                } else {
                    ""
                };
                if !hint.is_empty() {
                    ui.label(egui::RichText::new(hint).size(SMALL).color(MUTED));
                }
            });
        });
        if changed {
            self.save_prefs();
            self.refresh();
        }
        ui.add_space(4.0);

        if !self.db_ok {
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new(
                    "No tracks yet. Once a music folder is set up in Settings, the first scan \
                     fills this in.",
                )
                .size(BODY)
                .color(MUTED),
            );
            return;
        }

        let row_h = self.prefs.row_height;
        let sort = self.prefs.sort();
        let sort_desc = self.prefs.sort_desc;
        let table_gen = self.prefs.table_gen;
        let rows = self.rows.clone();
        let sel = self.sel.clone();
        let mut sort_clicked: Option<usize> = None;
        let mut action: Option<(Row, RowAction)> = None;
        let mut menu_open = false;

        let mut builder = TableBuilder::new(ui)
            // Part of the table's id, so "Reset column widths" is a matter of
            // bumping a number: the table then has no remembered widths to find.
            .id_salt(("tracks", table_gen))
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .min_scrolled_height(0.0)
            .auto_shrink([false, false]);
        for (i, (_, _, w)) in COLS.iter().enumerate() {
            builder = if i == COLS.len() - 1 {
                builder.column(Column::remainder().at_least(90.0).clip(true))
            } else {
                builder.column(Column::initial(*w).at_least(46.0).clip(true))
            };
        }

        builder
            .header(28.0, |mut header| {
                for (i, (name, col, _)) in COLS.iter().enumerate() {
                    header.col(|ui| {
                        let is_sort = sort.col == *col;
                        let text = egui::RichText::new(*name)
                            .size(TINY)
                            .strong()
                            .color(if is_sort { ACCENT } else { MUTED });
                        let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                        if is_sort {
                            sort_arrow(ui, resp.rect, sort_desc);
                        }
                        if resp.on_hover_text("sort by this column").clicked() {
                            sort_clicked = Some(i);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let idx = row.index();
                    let r = &rows[idx];
                    row.set_selected(sel.as_deref() == Some(r.path.as_str()));

                    row.col(|ui| {
                        pill(ui, &r.status, status_colour(&r.status));
                    });
                    row.col(|ui| cell(ui, egui::RichText::new(&r.artist).size(BODY).color(INK)));
                    row.col(|ui| cell(ui, egui::RichText::new(&r.album).size(BODY).color(MUTED)));
                    row.col(|ui| cell(ui, egui::RichText::new(&r.title).size(BODY).color(INK)));
                    row.col(|ui| {
                        cell(ui, egui::RichText::new(r.attempts.to_string()).size(SMALL).color(MUTED))
                    });
                    row.col(|ui| {
                        cell(ui, egui::RichText::new(due(&r.next_retry)).size(SMALL).color(MUTED))
                    });
                    row.col(|ui| {
                        cell(ui, egui::RichText::new(stamp(&r.last_attempt)).size(SMALL).color(MUTED))
                    });

                    // Actions carry the row itself, not its position: the
                    // list can be refreshed between the click and the act.
                    let resp = row.response();
                    if resp.clicked() {
                        action = Some((r.clone(), RowAction::Select));
                    }
                    if resp.double_clicked() {
                        action = Some((r.clone(), RowAction::Fix));
                    }
                    resp.on_hover_text(&r.path).context_menu(|ui| {
                        menu_open = true;
                        let mut pick = |a: RowAction| action = Some((r.clone(), a));
                        if ui.button("Find lyrics by hand").clicked() {
                            pick(RowAction::Fix);
                            ui.close_menu();
                        }
                        if ui.button("View the lyrics file").clicked() {
                            pick(RowAction::View);
                            ui.close_menu();
                        }
                        if ui.button("Show the track in Explorer").clicked() {
                            pick(RowAction::Folder);
                            ui.close_menu();
                        }
                        if ui.button("Copy the file path").clicked() {
                            pick(RowAction::Copy);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui
                            .button("Mark as instrumental")
                            .on_hover_text("stops Lyricarr ever asking about this track again")
                            .clicked()
                        {
                            pick(RowAction::Instrumental);
                            ui.close_menu();
                        }
                        if ui
                            .button("Forget it (check again next scan)")
                            .on_hover_text("removes the saved state for this one file")
                            .clicked()
                        {
                            pick(RowAction::Forget);
                            ui.close_menu();
                        }
                    });
                });
            });
        self.menu_open = menu_open;

        if let Some(i) = sort_clicked {
            if self.prefs.sort_col == i {
                self.prefs.sort_desc = !self.prefs.sort_desc;
            } else {
                self.prefs.sort_col = i;
                self.prefs.sort_desc = false;
            }
            self.save_prefs();
            self.refresh();
        }
        if let Some((r, act)) = action {
            self.row_action(r, act);
        }
    }

    fn row_action(&mut self, r: Row, act: RowAction) {
        self.sel = Some(r.path.clone());
        let audio = PathBuf::from(&r.path);
        match act {
            RowAction::Select => {}
            RowAction::Fix => self.open_fix(&r),
            RowAction::Copy => {
                self.notice = format!("copied {}", r.path);
                self.copy_pending = Some(r.path.clone());
            }
            RowAction::Folder => crate::os::reveal(&audio),
            RowAction::View => match tools::sidecar_of(&audio) {
                Some(p) => match tools::read_sidecar(&p) {
                    Some(t) => self.viewer = Some((p, t)),
                    None => self.notice = format!("could not read {}", p.display()),
                },
                None => self.notice = "there is no lyrics file beside that track yet".into(),
            },
            RowAction::Instrumental => match tools::mark_instrumental(&self.cfg, &audio) {
                Ok(()) => {
                    self.notice = format!("{} marked instrumental", r.title);
                    self.refresh();
                }
                Err(e) => self.notice = format!("could not mark it: {e:#}"),
            },
            RowAction::Forget => match tools::forget(&self.cfg, &audio) {
                Ok(()) => {
                    self.notice = format!("{} forgotten - the next scan will look again", r.title);
                    self.refresh();
                }
                Err(e) => self.notice = format!("could not forget it: {e:#}"),
            },
        }
    }

    fn lrc_viewer(&mut self, ctx: &egui::Context) {
        let Some((path, text)) = self.viewer.as_ref() else { return };
        let (path, text) = (path.clone(), text.clone());
        let mut open = true;
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "lyrics".into());
        egui::Window::new(title)
            .id(egui::Id::new("lrc-viewer"))
            .open(&mut open)
            .default_size([560.0, 540.0])
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{} lines", text.lines().count()))
                            .size(SMALL)
                            .color(MUTED),
                    );
                    if ui.button("Open in the text editor").clicked() {
                        let _ = opener::open(&path);
                    }
                    if ui
                        .button("Delete this file")
                        .on_hover_text("the next scan will look the track up again")
                        .clicked()
                    {
                        match std::fs::remove_file(&path) {
                            Ok(()) => {
                                self.notice = format!("deleted {}", path.display());
                                self.viewer = None;
                                self.refresh();
                            }
                            Err(e) => self.notice = format!("could not delete it: {e}"),
                        }
                    }
                });
                ui.separator();
                let lines: Vec<&str> = text.lines().collect();
                let h = ui.text_style_height(&egui::TextStyle::Monospace);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
                    ui,
                    h,
                    lines.len(),
                    |ui, range| {
                        for line in &lines[range] {
                            ui.label(egui::RichText::new(*line).monospace());
                        }
                    },
                );
            });
        if !open {
            self.viewer = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Find by hand
// ---------------------------------------------------------------------------

impl App {
    fn fix_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(
                "Ask LRCLIB yourself and choose the answer. This is for the tracks whose tags \
                 disagree with how LRCLIB spells the release - a scan has to be strict about \
                 what it accepts, you do not.",
            )
            .size(SMALL)
            .color(MUTED),
        );
        ui.add_space(8.0);

        if let Some(t) = self.fix.track.clone() {
            egui::Frame::none()
                .fill(CARD)
                .rounding(egui::Rounding::same(9.0))
                .inner_margin(egui::Margin::symmetric(12.0, 9.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        pill(ui, &t.status, status_colour(&t.status));
                        ui.label(
                            egui::RichText::new(format!(
                                "{}  -  {}  -  {}   ({})",
                                if t.artist.is_empty() { "(no artist)" } else { &t.artist },
                                if t.album.is_empty() { "(no album)" } else { &t.album },
                                if t.title.is_empty() { "(no title)" } else { &t.title },
                                mmss(t.duration),
                            ))
                            .size(BODY)
                            .strong(),
                        );
                    });
                    ui.label(egui::RichText::new(&t.path).size(SMALL).color(MUTED));
                    if !t.note.is_empty() {
                        ui.label(egui::RichText::new(&t.note).size(SMALL).color(MUTED));
                    }
                });
            ui.add_space(8.0);
        } else {
            ui.label(
                egui::RichText::new(
                    "Pick a track first: go to Tracks and double-click a row, or right-click it \
                     and choose \"Find lyrics by hand\".",
                )
                .size(BODY)
                .color(WARN),
            );
            ui.add_space(8.0);
        }

        let busy = self.fix.busy.load(Ordering::Relaxed);
        ui.horizontal(|ui| {
            ui.label("Artist");
            ui.add(egui::TextEdit::singleline(&mut self.fix.artist).desired_width(190.0));
            ui.label("Title");
            ui.add(egui::TextEdit::singleline(&mut self.fix.title).desired_width(240.0));
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("Search LRCLIB").fill(ACCENT.linear_multiply(0.55)),
                )
                .clicked()
            {
                self.search_lrclib();
            }
            if busy {
                ui.spinner();
                ui.label(egui::RichText::new("asking LRCLIB...").size(SMALL).color(MUTED));
            }
        });
        if !self.fix.done.is_empty() {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&self.fix.done).size(BODY).color(OK));
        }
        ui.add_space(8.0);

        let results = self.fix.results.lock().unwrap().clone();
        let Some(cands) = results else {
            if !busy {
                ui.label(egui::RichText::new("no search run yet").size(SMALL).color(MUTED));
            }
            return;
        };
        if cands.is_empty() {
            ui.label(
                egui::RichText::new(
                    "LRCLIB returned nothing for that. Try the title on its own, or the album \
                     artist instead of the track artist.",
                )
                .size(BODY)
                .color(WARN),
            );
            return;
        }

        let want = self.fix.track.as_ref().map(|t| t.duration).unwrap_or(0.0);
        let mut apply: Option<usize> = None;
        ui.label(
            egui::RichText::new(format!(
                "{} candidates - the one whose length matches your file is almost always right",
                cands.len()
            ))
            .size(SMALL)
            .color(MUTED),
        );
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, c) in cands.iter().enumerate() {
                    let close = want > 0.0 && (c.duration - want).abs() <= 5.0;
                    egui::Frame::none()
                        .fill(if self.fix.preview == Some(i) { CARD2 } else { CARD })
                        .rounding(egui::Rounding::same(9.0))
                        .inner_margin(egui::Margin::symmetric(12.0, 9.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if !c.synced.trim().is_empty() {
                                    pill(ui, "synced", OK);
                                } else if c.instrumental {
                                    pill(ui, "instrumental", MUTED);
                                } else {
                                    pill(ui, "plain", ACCENT);
                                }
                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} - {}",
                                            c.artist_name, c.track_name
                                        ))
                                        .size(BODY)
                                        .strong(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{}   {}   {}",
                                            if c.album_name.is_empty() {
                                                "(no album)"
                                            } else {
                                                &c.album_name
                                            },
                                            mmss(c.duration),
                                            if want > 0.0 {
                                                format!(
                                                    "{:+.0}s against your file",
                                                    c.duration - want
                                                )
                                            } else {
                                                String::new()
                                            },
                                        ))
                                        .size(SMALL)
                                        .color(if close { OK } else { MUTED }),
                                    );
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let can = self.fix.track.is_some()
                                            && !(c.synced.trim().is_empty()
                                                && c.plain.trim().is_empty());
                                        if ui
                                            .add_enabled(
                                                can,
                                                egui::Button::new("Use this one").fill(ACCENT),
                                            )
                                            .on_hover_text(
                                                "writes the sidecar and records it as chosen by hand",
                                            )
                                            .clicked()
                                        {
                                            apply = Some(i);
                                        }
                                        if ui.button("Preview").clicked() {
                                            self.fix.preview = if self.fix.preview == Some(i) {
                                                None
                                            } else {
                                                Some(i)
                                            };
                                        }
                                    },
                                );
                            });
                            if self.fix.preview == Some(i) {
                                ui.separator();
                                let text = if c.synced.trim().is_empty() {
                                    &c.plain
                                } else {
                                    &c.synced
                                };
                                egui::ScrollArea::vertical()
                                    .id_salt(("prev", i))
                                    .max_height(220.0)
                                    .show(ui, |ui| {
                                        for line in text.lines().take(400) {
                                            ui.label(egui::RichText::new(line).monospace());
                                        }
                                    });
                            }
                        });
                    ui.add_space(6.0);
                }
            });

        if let Some(i) = apply {
            if let Some(t) = self.fix.track.clone() {
                match tools::apply_match(&self.cfg, Path::new(&t.path), &cands[i]) {
                    Ok(dest) => {
                        self.fix.done = format!("wrote {}", dest.display());
                        self.notice = format!("wrote {}", dest.display());
                        self.refresh();
                    }
                    Err(e) => self.notice = format!("could not write the lyrics: {e:#}"),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Coverage and housekeeping
// ---------------------------------------------------------------------------

impl App {
    fn coverage_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .button("Retry every miss now")
                .on_hover_text("clears the retry windows on the notfound tracks")
                .clicked()
            {
                self.retry(store::ST_NOTFOUND);
            }
            if ui
                .button("Retry errors now")
                .on_hover_text("transport failures; these are already retried on the next run")
                .clicked()
            {
                self.retry(store::ST_ERROR);
            }
            if ui.button("Recount artists").clicked() {
                self.coverage_gen += 1;
                self.refresh();
            }
            let busy = self.stray_busy.load(Ordering::Relaxed);
            if ui
                .button(if busy { "Stop looking" } else { "Find stray lyrics files" })
                .on_hover_text("lyrics files with no matching audio file beside them")
                .clicked()
            {
                self.find_strays();
            }
            if busy {
                ui.spinner();
                ui.label(
                    egui::RichText::new("walking the library...")
                        .size(SMALL)
                        .color(MUTED),
                );
            }
        });
        ui.add_space(8.0);

        let strays = self.strays.lock().unwrap().clone();
        if let Some(list) = strays {
            let mut remove: Option<PathBuf> = None;
            egui::CollapsingHeader::new(format!("{} stray lyrics files", list.len()))
                .id_salt("strays-head")
                .default_open(true)
                .show(ui, |ui| {
                    if list.is_empty() {
                        ui.label(
                            egui::RichText::new("none - every lyrics file has its track")
                                .size(BODY)
                                .color(OK),
                        );
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("strays")
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for s in &list {
                                ui.horizontal(|ui| {
                                    if ui.small_button("delete").clicked() {
                                        remove = Some(s.lrc.clone());
                                    }
                                    if ui.small_button("open").clicked() {
                                        let _ = opener::open(&s.lrc);
                                    }
                                    ui.label(
                                        egui::RichText::new(bytes(s.bytes))
                                            .size(SMALL)
                                            .color(if s.bytes == 0 { BAD } else { MUTED }),
                                    );
                                    ui.label(
                                        egui::RichText::new(s.lrc.to_string_lossy())
                                            .size(SMALL)
                                            .color(MUTED),
                                    );
                                });
                            }
                        });
                });
            if let Some(p) = remove {
                match tools::delete_stray(&p) {
                    Ok(()) => {
                        self.notice = format!("deleted {}", p.display());
                        if let Some(v) = self.strays.lock().unwrap().as_mut() {
                            v.retain(|s| s.lrc != p);
                        }
                    }
                    Err(e) => self.notice = format!("could not delete it: {e}"),
                }
            }
            ui.add_space(8.0);
        }

        ui.label(
            egui::RichText::new(
                "Worst-covered artists first, counting only those with five tracks or more. \
                 These are where a targeted run pays off.",
            )
            .size(SMALL)
            .color(MUTED),
        );
        ui.add_space(4.0);
        let cov = self.cov.clone();
        let row_h = self.prefs.row_height;
        TableBuilder::new(ui)
            .id_salt(("coverage", self.prefs.table_gen))
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(300.0).at_least(120.0).clip(true))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::initial(110.0).at_least(60.0))
            .column(Column::remainder().at_least(140.0))
            .auto_shrink([false, false])
            .min_scrolled_height(0.0)
            .header(28.0, |mut h| {
                for name in ["ARTIST", "TRACKS", "WITH LYRICS", "COVERAGE"] {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(name).size(TINY).strong().color(MUTED));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, cov.len(), |mut row| {
                    let c = &cov[row.index()];
                    row.col(|ui| cell(ui, egui::RichText::new(&c.artist).size(BODY).color(INK)));
                    row.col(|ui| cell(ui, egui::RichText::new(fmt(c.total)).size(SMALL).color(MUTED)));
                    row.col(|ui| cell(ui, egui::RichText::new(fmt(c.done)).size(SMALL).color(MUTED)));
                    row.col(|ui| {
                        let p = c.pct() as f32 / 100.0;
                        ui.add(
                            egui::ProgressBar::new(p)
                                .desired_width(ui.available_width().min(260.0))
                                .fill(if p > 0.75 {
                                    OK
                                } else if p > 0.4 {
                                    WARN
                                } else {
                                    BAD
                                })
                                .text(egui::RichText::new(format!("{:.0}%", c.pct())).size(SMALL)),
                        );
                    });
                });
            });
    }

    fn activity_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Open the log file").clicked() {
                let _ = opener::open(&self.cfg.run.log_file);
            }
            if ui.button("Open the data folder").clicked() {
                let _ = opener::open(&self.data_dir);
            }
            ui.label(
                egui::RichText::new(format!("last {} lines", self.log_tail.len()))
                    .size(SMALL)
                    .color(MUTED),
            );
        });
        ui.add_space(6.0);
        egui::Frame::none()
            .fill(CARD)
            .rounding(egui::Rounding::same(9.0))
            .inner_margin(egui::Margin::symmetric(12.0, 8.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("New music").size(BODY).strong().color(ACCENT));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(self.monitor_summary()).size(SMALL).color(MUTED),
                        )
                        .truncate(),
                    );
                });
            });
        ui.add_space(6.0);
        // Only the lines on screen are laid out. All five hundred of them
        // used to be, on every frame.
        let lines = self.log_tail.clone();
        let h = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::vertical()
            .id_salt("log")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show_rows(ui, h, lines.len(), |ui, range| {
                for line in &lines[range] {
                    let colour = if line.contains("ERROR") {
                        BAD
                    } else if line.contains("WARN") {
                        WARN
                    } else if line.contains("monitor:") || line.contains("looked up ") {
                        INK
                    } else {
                        MUTED
                    };
                    ui.add(
                        egui::Label::new(egui::RichText::new(line).monospace().color(colour))
                            .truncate(),
                    );
                }
            });
    }

    /// The monitor in one sentence, for the Activity tab and Settings.
    fn monitor_summary(&self) -> String {
        let m = &self.mon;
        if m.passive {
            return "another Lyricarr (the background service) is watching - this window \
                    leaves it to that one"
                .into();
        }
        if !m.watch {
            return "live monitoring is off - new music waits for the next scan".into();
        }
        if m.folders.is_empty() {
            return "no music folders to watch yet".into();
        }
        let mut parts = vec![format!(
            "watching {} of {}",
            m.watching(),
            plural(m.folders.len(), "folder", "folders")
        )];
        if m.checking > 0 {
            parts.push(format!("looking up {} now", plural(m.checking, "new item", "new items")));
        }
        if m.waiting > 0 {
            parts.push(format!("{} waiting to finish copying", m.waiting));
        }
        if m.checking == 0 && m.waiting == 0 {
            parts.push("nothing waiting".into());
        }
        if let Some((t, n)) = m.last_batch {
            parts.push(format!(
                "last: {} at {}",
                plural(n, "item", "items"),
                t.format("%H:%M")
            ));
        }
        parts.join("   -   ")
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

impl App {
    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("settings")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut prefs_changed = false;
                let mut monitor_changed = false;

                self.folders_section(ui);

                section(ui, "Automation", |ui| {
                    if ui
                        .checkbox(
                            &mut self.prefs.watch_folders,
                            "Watch the music folders and fetch lyrics for new songs as they arrive",
                        )
                        .on_hover_text(
                            "Anything that lands in a music folder - an album copied in by hand, \
                             a download, a sync - is looked up a few seconds after it finishes \
                             copying.",
                        )
                        .changed()
                    {
                        monitor_changed = true;
                    }
                    ui.indent("watch-detail", |ui| {
                        ui.label(
                            egui::RichText::new(if cfg!(windows) {
                                "Works alongside the Lidarr hook: whichever reaches a track first \
                                 fetches its lyrics and the other skips it. Watching runs while \
                                 Lyricarr is open or in the tray."
                            } else {
                                "Works alongside the Lidarr hook: whichever reaches a track first \
                                 fetches its lyrics and the other skips it. Watching runs while \
                                 this window is open, or Lyricarr is running in the background."
                            })
                            .size(SMALL)
                            .color(MUTED),
                        );
                        ui.add_enabled_ui(self.prefs.watch_folders, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Wait for a new file to finish copying");
                                let mut secs = self.prefs.watch_settle_secs;
                                let r = ui
                                    .add(
                                        egui::Slider::new(&mut secs, 3..=120)
                                            .suffix(" s")
                                            .logarithmic(true),
                                    )
                                    .on_hover_text(
                                        "How long a new file must be left alone before it is \
                                         read. Raise it if music arrives over a slow network.",
                                    );
                                if r.changed() {
                                    self.prefs.watch_settle_secs = secs;
                                }
                                // Applied when the slider is let go, rather
                                // than on every step while dragging.
                                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                                    monitor_changed = true;
                                }
                            });
                        });
                        let (line, colour) = (self.monitor_summary(), MUTED);
                        ui.label(egui::RichText::new(line).size(SMALL).color(colour));
                    });

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("Full scan automatically");
                        let current = AUTO_SCAN_CHOICES
                            .iter()
                            .find(|(h, _)| *h == self.prefs.auto_scan_hours)
                            .map(|(_, l)| l.to_string())
                            .unwrap_or_else(|| {
                                format!("Every {} hours", self.prefs.auto_scan_hours)
                            });
                        egui::ComboBox::from_id_salt("autoscan")
                            .selected_text(current)
                            .show_ui(ui, |ui| {
                                for (h, label) in AUTO_SCAN_CHOICES {
                                    if ui
                                        .selectable_value(&mut self.prefs.auto_scan_hours, h, label)
                                        .changed()
                                    {
                                        monitor_changed = true;
                                    }
                                }
                            });
                        if self.prefs.auto_scan_hours > 0 {
                            if let Some(next) = self.mon.next_full_scan {
                                let secs = (next - chrono::Utc::now()).num_seconds();
                                let text = if self.full_live() {
                                    "a full scan is running now".to_string()
                                } else if secs <= 0 {
                                    "due now - starts within a minute".to_string()
                                } else {
                                    format!("next one in {}", span(secs))
                                };
                                ui.label(egui::RichText::new(text).size(SMALL).color(MUTED));
                            }
                        }
                    });
                    ui.indent("auto-detail", |ui| {
                        ui.label(
                            egui::RichText::new(
                                "The safety net under the watching: picks up music added while \
                                 Lyricarr was closed or a drive was disconnected, and re-asks \
                                 about misses once their retry time is up. Tracks that already \
                                 have lyrics cost nothing.",
                            )
                            .size(SMALL)
                            .color(MUTED),
                        );
                    });
                    ui.add_space(6.0);
                    prefs_changed |= ui
                        .checkbox(
                            &mut self.prefs.scan_on_start,
                            "Catch up when Lyricarr starts - scan for anything added while it \
                             was closed",
                        )
                        .changed();

                    // Watching only happens while Lyricarr is running.
                    if self.prefs.watch_folders && !self.start_with_windows && !self.mon.passive {
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(if cfg!(windows) {
                                    "Lyricarr does not start with Windows, so after a restart \
                                     nothing is watched until you open it."
                                } else {
                                    "Lyricarr does not start when you log in, so once this \
                                     window is closed nothing is watched."
                                })
                                .size(SMALL)
                                .color(WARN),
                            );
                            let label =
                                if cfg!(windows) { "Start with Windows" } else { "Start at log-in" };
                            if ui.small_button(label).clicked() {
                                prefs_changed |= self.set_start_with_windows(true);
                            }
                        });
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.label(
                        egui::RichText::new(if cfg!(windows) {
                            "Or leave it to Windows: a scheduled task runs a scan whether or not \
                             Lyricarr is open, even with nobody signed in."
                        } else {
                            "Or leave it to systemd: a user timer runs a scan whether or not \
                             Lyricarr is open. For watching with no desktop at all, see \
                             `lyricarr install-service`."
                        })
                        .size(SMALL)
                        .color(MUTED),
                    );
                    let busy = self.task_busy.load(Ordering::Relaxed);
                    ui.horizontal(|ui| {
                        let what = if cfg!(windows) {
                            "Install the weekly scheduled task"
                        } else {
                            "Install the weekly systemd timer"
                        };
                        if ui
                            .add_enabled(!busy, egui::Button::new(what))
                            .on_hover_text("a scan every Sunday at 04:00")
                            .clicked()
                        {
                            self.install_task(false);
                        }
                        if ui.add_enabled(!busy, egui::Button::new("Remove it")).clicked() {
                            self.install_task(true);
                        }
                        if busy {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new(format!(
                                    "asking {}...",
                                    crate::os::SCHEDULER_NAME
                                ))
                                .size(SMALL)
                                .color(MUTED),
                            );
                        }
                    });
                });

                section(ui, if crate::os::HAS_TRAY { "Window and tray" } else { "Window" }, |ui| {
                    if !crate::os::HAS_TRAY {
                        ui.label(
                            egui::RichText::new(
                                "There is no tray on Linux. Closing the window quits it; to \
                                 keep watching for new music afterwards, start Lyricarr at \
                                 log-in - it then runs in the background with no window, and \
                                 this window leaves the watching to it whenever it is open.",
                            )
                            .size(SMALL)
                            .color(MUTED),
                        );
                    }
                    if crate::os::HAS_TRAY {
                        prefs_changed |= ui
                            .checkbox(
                                &mut self.prefs.close_to_tray,
                                "Closing the window keeps Lyricarr running in the tray",
                            )
                            .on_hover_text(
                                "Off means the X button quits - and stops watching for new music. \
                                 Either way a scan already running is a separate process and \
                                 finishes on its own.",
                            )
                            .changed();
                        if ui
                            .checkbox(
                                &mut self.prefs.minimize_to_tray,
                                "Minimising hides to the tray instead of the taskbar",
                            )
                            .changed()
                        {
                            // The tray thread is what acts on this, so tell it.
                            self.shared.minimize_to_tray.store(
                                self.prefs.minimize_to_tray,
                                Ordering::Relaxed,
                            );
                            prefs_changed = true;
                        }
                    }
                    let mut on = self.start_with_windows;
                    if ui
                        .checkbox(&mut on, crate::os::START_AT_LOGIN_LABEL)
                        .on_hover_text(if cfg!(windows) {
                            "a per-user entry under HKCU ...\\CurrentVersion\\Run"
                        } else {
                            "an entry in ~/.config/autostart, which GNOME, KDE, Xfce and the \
                             rest all read"
                        })
                        .changed()
                    {
                        prefs_changed |= self.set_start_with_windows(on);
                    }
                });

                section(ui, "Looking lyrics up", |ui| {
                    egui::Grid::new("engine")
                        .num_columns(2)
                        .spacing([14.0, 8.0])
                        .show(ui, |ui| {
                            field(
                                ui,
                                "requests per second",
                                &mut self.draft.rps,
                                "LRCLIB is free and run by volunteers; 3 is polite and plenty.",
                            );
                            field(
                                ui,
                                "parallel lookups",
                                &mut self.draft.workers,
                                "More threads do not help once the rate limit is the bottleneck.",
                            );
                            field(
                                ui,
                                "timeout (seconds)",
                                &mut self.draft.timeout,
                                "A timeout is recorded as an error and retried, never as \
                                 'no lyrics'.",
                            );
                            ui.label("unsynced lyrics");
                            egui::ComboBox::from_id_salt("plain")
                                .selected_text(self.draft.plain.clone())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.draft.plain,
                                        "lrc".into(),
                                        "write a .lrc",
                                    );
                                    ui.selectable_value(
                                        &mut self.draft.plain,
                                        "txt".into(),
                                        "write a .txt",
                                    );
                                    ui.selectable_value(
                                        &mut self.draft.plain,
                                        "skip".into(),
                                        "skip them",
                                    );
                                });
                            ui.end_row();
                            field(
                                ui,
                                "shortest lyrics to keep",
                                &mut self.draft.min_chars,
                                "Characters. Guards against one-line junk entries.",
                            );
                            ui.label("overwrite existing files");
                            ui.checkbox(&mut self.draft.overwrite, "").on_hover_text(
                                "Off means a lyrics file already beside the track is left alone.",
                            );
                            ui.end_row();
                            ui.label("write [instrumental] files");
                            ui.checkbox(&mut self.draft.instrumental_marker, "")
                                .on_hover_text("Some players want a marker file; most do not.");
                            ui.end_row();
                        });

                    ui.add_space(6.0);
                    egui::CollapsingHeader::new("Matching thresholds (advanced)")
                        .id_salt("thresholds-head")
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(
                                    "These were calibrated against the previous Python version \
                                     so that the rewrite finds exactly the same matches. Lowering \
                                     them finds more, and some of what it finds will be the wrong \
                                     song.",
                                )
                                .size(SMALL)
                                .color(WARN),
                            );
                            ui.add_space(4.0);
                            egui::Grid::new("thresholds")
                                .num_columns(2)
                                .spacing([14.0, 8.0])
                                .show(ui, |ui| {
                                    field(
                                        ui,
                                        "minimum combined score",
                                        &mut self.draft.min_score,
                                        "",
                                    );
                                    field(
                                        ui,
                                        "minimum title likeness",
                                        &mut self.draft.min_title_ratio,
                                        "",
                                    );
                                    field(
                                        ui,
                                        "minimum title likeness (unsynced)",
                                        &mut self.draft.plain_min_title_ratio,
                                        "",
                                    );
                                });
                        });

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Save to config.toml")
                                        .color(egui::Color32::BLACK)
                                        .strong(),
                                )
                                .fill(ACCENT),
                            )
                            .clicked()
                        {
                            self.save_engine_settings();
                        }
                        if ui.button("Discard changes").clicked() {
                            self.draft = Draft::from(&self.cfg);
                        }
                        if ui.button("Open config.toml").clicked() {
                            let _ = opener::open(self.cfg_path());
                        }
                    });
                    ui.label(
                        egui::RichText::new(
                            "Saving rewrites only the lines that changed, so the comments in that \
                             file survive. A scan already running keeps the settings it started \
                             with.",
                        )
                        .size(SMALL)
                        .color(MUTED),
                    );
                });

                section(ui, "Readability", |ui| {
                    let mut scale = self.prefs.ui_scale;
                    let r = ui.add(
                        egui::Slider::new(&mut scale, 0.8..=1.6)
                            .text("overall size")
                            .fixed_decimals(2),
                    );
                    if r.changed() {
                        self.prefs.ui_scale = scale;
                    }
                    // Applied when the slider is let go. Rescaling while it
                    // was being dragged moved the slider out from under the
                    // pointer, so the value jumped about and the whole window
                    // shook.
                    if r.drag_stopped() || (r.changed() && !r.dragged()) {
                        ui.ctx().set_zoom_factor(self.prefs.ui_scale);
                        prefs_changed = true;
                    }
                    prefs_changed |= ui
                        .add(
                            egui::Slider::new(&mut self.prefs.row_height, 18.0..=44.0)
                                .text("table row height")
                                .fixed_decimals(0),
                        )
                        .changed();
                    let mut limit = self.prefs.row_limit as u32;
                    if ui
                        .add(
                            egui::Slider::new(&mut limit, 50..=5000)
                                .text("rows to load")
                                .logarithmic(true),
                        )
                        .on_hover_text("how many rows the Tracks table asks the database for")
                        .changed()
                    {
                        self.prefs.row_limit = limit as usize;
                        prefs_changed = true;
                        self.refresh();
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Reset column widths").clicked() {
                            self.prefs.table_gen = self.prefs.table_gen.wrapping_add(1);
                            prefs_changed = true;
                            self.notice = "columns back to their default widths".into();
                        }
                        if ui.button("Reset all window settings").clicked() {
                            let gen = self.prefs.table_gen.wrapping_add(1);
                            let start = self.prefs.start_with_windows;
                            // The automation settings are not "window
                            // settings"; resetting the layout must not
                            // quietly switch monitoring back on or off.
                            let keep = (
                                self.prefs.watch_folders,
                                self.prefs.watch_settle_secs,
                                self.prefs.auto_scan_hours,
                                self.prefs.scan_on_start,
                            );
                            self.prefs = Prefs::default();
                            self.prefs.table_gen = gen;
                            self.prefs.start_with_windows = start;
                            (
                                self.prefs.watch_folders,
                                self.prefs.watch_settle_secs,
                                self.prefs.auto_scan_hours,
                                self.prefs.scan_on_start,
                            ) = keep;
                            self.shared.minimize_to_tray.store(
                                self.prefs.minimize_to_tray,
                                Ordering::Relaxed,
                            );
                            ui.ctx().set_zoom_factor(self.prefs.ui_scale);
                            prefs_changed = true;
                            self.refresh();
                        }
                    });
                });

                if monitor_changed {
                    self.reconfigure_monitor();
                    prefs_changed = true;
                }
                if prefs_changed {
                    self.save_prefs();
                }
            });
    }

    /// Turn the sign-in entry on or off. Returns whether the preference changed.
    fn set_start_with_windows(&mut self, on: bool) -> bool {
        match crate::os::set_start_at_login(on) {
            Ok(()) => {
                self.start_with_windows = on;
                self.prefs.start_with_windows = on;
                self.notice = match (on, cfg!(windows)) {
                    (true, true) => "Lyricarr will start with Windows, in the tray".into(),
                    (false, true) => "Lyricarr will no longer start with Windows".into(),
                    (true, false) => "Lyricarr will start in the background when you log in".into(),
                    (false, false) => "Lyricarr will no longer start when you log in".into(),
                };
                true
            }
            Err(e) => {
                self.notice = format!("could not change the sign-in entry: {e}");
                false
            }
        }
    }

    /// Add a music folder - picked or typed - and start looking through it.
    /// Returns whether it was added.
    fn add_root(&mut self, dir: PathBuf) -> bool {
        if !dir.is_absolute() {
            self.notice = format!(
                "{} is not a full path - it should start with {}",
                dir.display(),
                if cfg!(windows) { "a drive letter, like D:\\" } else { "/" }
            );
            return false;
        }
        if !dir.is_dir() {
            self.notice = format!("{} is not a folder Lyricarr can reach right now", dir.display());
            return false;
        }
        let s = dir.to_string_lossy().to_string();
        if self.cfg.library.roots.iter().any(|r| crate::os::same_path(r, &s)) {
            self.notice = "that folder is already in the list".into();
            return false;
        }
        let mut next = self.cfg.library.roots.clone();
        next.push(s);
        if !self.save_roots(next) {
            return false;
        }
        // Look through what is already in it straight away, rather than
        // leaving that for whenever the next full scan comes round.
        if !self.full_live() {
            self.scan_folder(&dir);
            self.notice = format!("added {} - looking through it now", dir.display());
        }
        true
    }

    fn folders_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Music folders", |ui| {
            ui.label(
                egui::RichText::new(
                    "Every scan walks these, and the monitor watches them. Add as many as you \
                     like - a second drive, an archive, anything Lidarr does not manage.",
                )
                .size(SMALL)
                .color(MUTED),
            );
            ui.add_space(6.0);

            let mut drop_at: Option<usize> = None;
            let mut scan_one: Option<PathBuf> = None;
            let mut add: Option<PathBuf> = None;
            let roots = self.cfg.library.roots.clone();
            if roots.is_empty() {
                ui.label(
                    egui::RichText::new("no folders yet - add the one Lidarr files your music into")
                        .size(BODY)
                        .color(BAD),
                );
            }
            for (i, r) in roots.iter().enumerate() {
                let here = PathBuf::from(r);
                let reachable = here.is_dir();
                let watch = self
                    .mon
                    .folders
                    .iter()
                    .find(|f| f.path == *r)
                    .map(|f| f.state);
                ui.push_id(("root", i), |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("remove")
                            .on_hover_text("stops scanning it; nothing on disk is touched")
                            .clicked()
                        {
                            drop_at = Some(i);
                        }
                        if ui.small_button("open").clicked() {
                            let _ = opener::open(&here);
                        }
                        if ui
                            .small_button("scan just this")
                            .on_hover_text("runs now, even if the full scan is running")
                            .clicked()
                        {
                            scan_one = Some(here.clone());
                        }
                        let (badge, colour, tip) = match (self.prefs.watch_folders, watch) {
                            _ if self.mon.passive => (
                                "service",
                                ACCENT,
                                "watched by the background service",
                            ),
                            (false, _) => ("not watched", MUTED, "live monitoring is off"),
                            (true, Some(watch::FolderState::Partial)) => (
                                "partly watched",
                                WARN,
                                "Linux ran out of inotify watches part-way through this folder. \
                                 Raise the limit with\n  sudo sysctl fs.inotify.max_user_watches=524288\n\
                                 and add that line to a file in /etc/sysctl.d/ to keep it.",
                            ),
                            (true, Some(watch::FolderState::Watching)) => (
                                "watching",
                                ACCENT,
                                "new music here gets its lyrics as it lands",
                            ),
                            (true, Some(watch::FolderState::Unreachable)) => (
                                "offline",
                                BAD,
                                "not reachable right now - Lyricarr keeps trying",
                            ),
                            (true, _) => ("starting", MUTED, "setting up the watch"),
                        };
                        pill(ui, badge, colour).on_hover_text(tip);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(r)
                                    .size(BODY)
                                    .color(if reachable { INK } else { BAD }),
                            )
                            .truncate(),
                        )
                        .on_hover_text(if reachable {
                            r.to_string()
                        } else {
                            "Not reachable right now. A mapped drive that is not connected \
                             looks exactly like an empty folder to a scan, so it is called out \
                             here."
                                .to_string()
                        });
                    });
                });
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let picking = self.picker.busy.load(Ordering::Relaxed);
                if ui
                    .add_enabled(
                        !picking,
                        egui::Button::new(
                            egui::RichText::new("Add folder...")
                                .color(egui::Color32::BLACK)
                                .strong(),
                        )
                        .fill(ACCENT),
                    )
                    .clicked()
                {
                    let start = self
                        .cfg
                        .library
                        .roots
                        .first()
                        .map(PathBuf::from)
                        .filter(|p| p.is_dir())
                        .unwrap_or_else(crate::os::default_browse_dir);
                    self.picker.open(
                        ui.ctx(),
                        Pick::AddRoot,
                        "Choose a music folder to scan",
                        Some(start),
                    );
                }
                if ui
                    .button("Scan one folder now...")
                    .on_hover_text("pick any folder - it does not have to be in the list above")
                    .clicked()
                {
                    self.picker.open(ui.ctx(), Pick::ScanOne, "Choose a folder to scan now", None);
                }
                if ui.button("Open the data folder").clicked() {
                    let _ = opener::open(&self.data_dir);
                }
            });
            // Typing or pasting a path works where a picker does not: a
            // network path, or a Linux machine with no desktop portal to show
            // a folder picker at all.
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.add_path)
                        .hint_text("or type or paste a folder path")
                        .desired_width(340.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let can = !self.add_path.trim().is_empty();
                if (ui.add_enabled(can, egui::Button::new("Add")).clicked() || enter) && can {
                    add = Some(PathBuf::from(self.add_path.trim()));
                }
            });
            if let Some(dir) = add {
                if self.add_root(dir) {
                    self.add_path.clear();
                }
            }

            if let Some(i) = drop_at {
                let mut next = self.cfg.library.roots.clone();
                if i < next.len() {
                    next.remove(i);
                }
                self.save_roots(next);
            }
            if let Some(d) = scan_one {
                self.scan_folder(&d);
            }

            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!(
                    "audio files: {}",
                    self.cfg.library.audio_extensions.join("  ")
                ))
                .size(SMALL)
                .color(MUTED),
            );
            ui.label(
                egui::RichText::new(
                    "Changing the folders rewrites one line of config.toml and leaves its \
                     comments alone. A scan already running keeps the folders it started with.",
                )
                .size(SMALL)
                .color(MUTED),
            );
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    // The title is the id, so the widgets inside a section keep the same ids
    // whatever is drawn above them.
    ui.push_id(title, |ui| {
        egui::Frame::none()
            .fill(CARD)
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::symmetric(14.0, 12.0))
            .show(ui, |ui| {
                // Without this each card is only as wide as its own contents,
                // which leaves the column of them visibly ragged.
                ui.set_width(ui.available_width().min(820.0));
                ui.label(egui::RichText::new(title).size(16.0).strong().color(ACCENT));
                ui.add_space(6.0);
                body(ui);
            });
    });
    ui.add_space(6.0);
}

/// One labelled text box in a two-column grid, with the same tooltip on both
/// halves so it does not matter which one the pointer lands on.
fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    let l = ui.label(label);
    if !hint.is_empty() {
        l.on_hover_text(hint);
    }
    let e = ui.add(egui::TextEdit::singleline(value).desired_width(90.0));
    if !hint.is_empty() {
        e.on_hover_text(hint);
    }
    ui.end_row();
}

// ---------------------------------------------------------------------------
// formatting
// ---------------------------------------------------------------------------

fn fmt(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn bytes(n: u64) -> String {
    if n == 0 {
        "empty".into()
    } else if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.0} KB", n as f64 / 1024.0)
    }
}

fn mmss(secs: f64) -> String {
    if secs <= 0.0 {
        return "?:??".into();
    }
    let t = secs.round() as i64;
    format!("{}:{:02}", t / 60, t % 60)
}

/// "in 6d" / "due" - what the Retry column showed in the old window.
fn due(iso: &str) -> String {
    match store::parse_iso(iso) {
        None => String::new(),
        Some(t) => {
            let d = (t - chrono::Utc::now()).num_days();
            if d < 0 {
                "due".into()
            } else if d == 0 {
                "today".into()
            } else {
                format!("in {d}d")
            }
        }
    }
}

/// How long ago, in the roughest terms that are still useful.
fn stamp(iso: &str) -> String {
    match store::parse_iso(iso) {
        None => String::new(),
        Some(t) => {
            let mins = (chrono::Utc::now() - t).num_minutes();
            if mins < 2 {
                "just now".into()
            } else if mins < 60 {
                format!("{mins} min ago")
            } else if mins < 60 * 36 {
                format!("{}h ago", mins / 60)
            } else {
                format!("{}d ago", mins / 1440)
            }
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", fmt(n as i64), if n == 1 { one } else { many })
}

/// A length of time in the two largest units that matter: "45s", "12 min",
/// "3h 05m", "2d 4h".
fn span(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86_400 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    }
}

/// "just now", "3h 05m ago".
fn ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - t).num_seconds();
    if secs < 90 {
        "just now".into()
    } else {
        format!("{} ago", span(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_naturally() {
        assert_eq!(span(-5), "0s");
        assert_eq!(span(45), "45s");
        assert_eq!(span(12 * 60 + 30), "12 min");
        assert_eq!(span(3 * 3600 + 5 * 60), "3h 05m");
        assert_eq!(span(2 * 86_400 + 4 * 3600 + 59), "2d 4h");
    }

    #[test]
    fn plurals_and_thousands() {
        assert_eq!(plural(1, "folder", "folders"), "1 folder");
        assert_eq!(plural(2, "folder", "folders"), "2 folders");
        assert_eq!(plural(12_345, "track", "tracks"), "12,345 tracks");
    }
}
