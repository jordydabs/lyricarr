//! Live monitoring: new music gets its lyrics as it lands, not at the next scan.
//!
//! The Lidarr hook only hears about what Lidarr itself imports. Anything else -
//! an album copied in by hand, a download client writing straight into the
//! library, a folder synced from another machine - used to wait for the weekly
//! sweep. This watches every music folder with `ReadDirectoryChangesW` and
//! hands what arrives to a targeted `lyricarr scan`, the same run the hook
//! makes, so the two are safe side by side: whichever gets to a track second
//! finds it done and skips it.
//!
//! The same thread also runs the automatic full scan on its schedule. That is
//! the safety net under both: anything the watcher could not see - a drive that
//! was disconnected, more changes at once than its buffer holds, music added
//! while Lyricarr was not running - is found on the next full pass.
//!
//! It lives on threads of its own rather than in the window's frame loop,
//! because a window hidden in the tray gets no frames at all (see `winui.rs`).
//!
//! Only one monitor is active at a time, whichever process holds
//! `monitor.lock`: the window, or `lyricarr watch` / the Linux background
//! service. Any other waits, and takes over if the active one goes away. Two
//! at once would each hand the same new album to a scan of its own.

use crate::config::Config;
use crate::logging::Log;
use crate::store::{self, Store};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Something that has been waiting this long is given up on. The next full
/// scan will find it; holding it forever would only keep a line in the window.
const MAX_WAIT: Duration = Duration::from_secs(60 * 60);

/// The longest a folder is kept back for a file inside it that is still busy,
/// or a file for the busy folder it sits in. Past this, whatever is ready
/// goes out on its own: a track some other program keeps locked must not hold
/// up the rest of its folder - or, during a rescan of a whole music folder,
/// all new music - for the full hour, and neither must a folder that a
/// download client keeps writing into.
const HOLD_MAX: Duration = Duration::from_secs(10 * 60);

/// How often the schedule for the automatic full scan is looked at.
const SCHEDULE_EVERY: Duration = Duration::from_secs(30);

/// After starting an automatic full scan, leave the schedule alone this long.
/// A scan that exits at once - because another program holds the scan lock
/// without keeping its heartbeat fresh - would otherwise be relaunched on
/// every check.
const RELAUNCH_GAP: Duration = Duration::from_secs(10 * 60);

/// What the monitor is told to do. Rebuilt from the window's preferences and
/// `config.toml` whenever either changes.
#[derive(Clone)]
pub struct Settings {
    /// Watch the music folders live.
    pub watch: bool,
    /// How long a new file must be left alone before it is looked up.
    pub settle: Duration,
    /// Run a full scan this often; 0 is never.
    pub auto_scan_hours: u32,
    pub cfg: Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderState {
    Starting,
    Watching,
    /// Linux: watching, but not every subfolder - the kernel's limit on
    /// inotify watches ran out. What is not watched waits for a full scan.
    Partial,
    /// Not there right now - an unmapped drive, a NAS that is asleep, a USB
    /// drive that was ejected. Retried.
    Unreachable,
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub path: String,
    pub state: FolderState,
}

/// A snapshot of the monitor for the window and the tray.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub watch: bool,
    pub folders: Vec<Folder>,
    /// Arrived, waiting until they stop changing.
    pub waiting: usize,
    /// Being looked up right now.
    pub checking: usize,
    /// When the last batch finished, and how many files and folders it held.
    pub last_batch: Option<(chrono::DateTime<chrono::Local>, usize)>,
    pub auto_scan_hours: u32,
    /// When the next automatic full scan is due, if one is scheduled.
    pub next_full_scan: Option<chrono::DateTime<chrono::Utc>>,
    /// Another Lyricarr holds `monitor.lock` and is doing the watching and
    /// the automatic scans; this one is waiting its turn.
    pub passive: bool,
}

impl Status {
    pub fn watching(&self) -> usize {
        self.folders
            .iter()
            .filter(|f| matches!(f.state, FolderState::Watching | FolderState::Partial))
            .count()
    }
    pub fn partial(&self) -> usize {
        self.folders.iter().filter(|f| f.state == FolderState::Partial).count()
    }
    pub fn unreachable(&self) -> usize {
        self.folders.iter().filter(|f| f.state == FolderState::Unreachable).count()
    }
}

enum Msg {
    Configure(Box<Settings>),
    /// Look at the full-scan schedule now rather than at the next check.
    CheckNow,
    Shutdown,
    // From the folder watchers. `gen` says which set of watchers sent it, so
    // a late message from one being replaced cannot land on its successor.
    Changed { gen: u64, path: PathBuf, appeared: bool },
    Missed { gen: u64, root: usize, why: &'static str },
    State { gen: u64, root: usize, state: FolderState },
    /// Windows: the watch let go of a folder so its drive could be ejected.
    #[cfg_attr(not(windows), allow(dead_code))]
    Released { gen: u64, root: usize },
}

#[cfg(windows)]
use win as platform;
#[cfg(target_os = "linux")]
use linux as platform;

/// The handle the window keeps. Everything it does is a message to the
/// monitor's own thread, so none of it can stall a frame.
pub struct Monitor {
    tx: Sender<Msg>,
    status: Arc<Mutex<Status>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Monitor {
    pub fn start(settings: Settings, log: Log) -> Arc<Self> {
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let co = Coordinator {
            settings: settings.clone(),
            log,
            tx: tx.clone(),
            gen: 0,
            watchers: Vec::new(),
            folders: Vec::new(),
            lost: Vec::new(),
            pending: Pending::default(),
            batch: None,
            last_batch: None,
            full_child: None,
            full_launched: None,
            next_full: None,
            db: None,
            lock: None,
            last_lock_try: Instant::now(),
            status: status.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("monitor".into())
            .spawn(move || co.run(rx, settings))
            .ok();
        Arc::new(Self { tx, status, thread: Mutex::new(thread) })
    }

    pub fn configure(&self, settings: Settings) {
        let _ = self.tx.send(Msg::Configure(Box::new(settings)));
    }

    pub fn check_now(&self) {
        let _ = self.tx.send(Msg::CheckNow);
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Stop watching. Lookups already handed to a scan process carry on and
    /// finish by themselves, exactly as a scan started from the window does.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.lock().ok().and_then(|mut g| g.take()) {
            let _ = t.join();
        }
    }
}

/// A change worth acting on: an audio file, or a folder that has just
/// appeared - an album moved into the library arrives as one event for the
/// folder and none at all for what is inside it.
///
/// A folder that is merely *modified* is ignored. Every file written inside
/// a folder modifies it, including each lyrics file Lyricarr writes, so
/// following those would have every lookup trigger another walk of its album.
fn interesting(cfg: &Config, path: &Path, appeared: bool) -> bool {
    if path.components().any(|c| cfg.skips_dir(&c.as_os_str().to_string_lossy())) {
        return false;
    }
    cfg.is_audio(path) || (appeared && path.is_dir())
}

// ---------------------------------------------------------------------------
// waiting for files to settle
// ---------------------------------------------------------------------------

/// Size and modification time - of a file, or summed over a folder.
type Sig = (u64, Option<SystemTime>);

/// What a look at a waiting path found.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Probe {
    Gone,
    /// A folder, with a signature of everything in it - or none when it is
    /// too big to be worth summing.
    Dir { sig: Option<Sig> },
    /// Size and modification time, and whether it can be opened yet.
    File { sig: Sig, open: bool },
}

/// Past this many entries a folder is not summed. That is a whole library
/// root being rescanned after missed changes, a long walk in itself.
const FOLDER_SIG_LIMIT: usize = 2000;

/// Everything under a folder boiled down to one signature, so a folder gets
/// the same "has it stopped changing" test as a single file.
///
/// Event timing alone is not enough. Windows reports a file's growth lazily -
/// often only when the writer closes it - so an album still being copied in
/// can sit there for longer than the settle time without a single event, and
/// a walk of it then reads half a file with the wrong duration. Sizes come
/// from a stat of each file, not from the directory listing, which can lag
/// behind a file that is still open for writing.
fn folder_sig(dir: &Path) -> Option<Sig> {
    let (mut total, mut newest, mut seen) = (0u64, None::<SystemTime>, 0usize);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > FOLDER_SIG_LIMIT {
                return None;
            }
            let p = e.path();
            let Ok(md) = std::fs::metadata(&p) else { continue };
            if md.is_dir() {
                stack.push(p);
            } else {
                // +1 per file, so a new empty file still changes the sum.
                total = total.wrapping_add(md.len()).wrapping_add(1);
                newest = newest.max(md.modified().ok());
            }
        }
    }
    Some((total, newest))
}

fn probe(p: &Path) -> Probe {
    match std::fs::metadata(p) {
        Err(_) => Probe::Gone,
        Ok(md) if md.is_dir() => Probe::Dir { sig: folder_sig(p) },
        Ok(md) => Probe::File {
            sig: (md.len(), md.modified().ok()),
            // A file still being copied in is usually held open without read
            // sharing, which is the surest sign it is not finished.
            open: std::fs::File::open(p).is_ok(),
        },
    }
}

struct Wait {
    first: Instant,
    last_event: Instant,
    /// Size and mtime the last time it was looked at.
    sig: Option<Sig>,
}

/// Paths that changed and have not been looked up yet.
///
/// Reading a half-copied file is worse than useless: its duration is wrong,
/// so a lookup either misses or - worse - matches a different cut, and the
/// sidecar it writes is then protected from correction by `overwrite = false`.
/// So nothing leaves here until it has been left alone for the settle time
/// *and* has stopped growing between two looks *and* can be opened.
#[derive(Default)]
struct Pending {
    items: HashMap<PathBuf, Wait>,
    /// The folder entries among `items`, kept apart so that bumping them is
    /// not a pass over every waiting file.
    dirs: HashSet<PathBuf>,
}

impl Pending {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn clear(&mut self) {
        self.items.clear();
        self.dirs.clear();
    }

    fn note(&mut self, path: PathBuf, is_dir: bool, now: Instant) {
        // A folder is not ready while anything inside it is still changing:
        // one copied in arrives as the folder first, then each of its files
        // as they are written. Any size remembered from before this event is
        // out of date, so it is forgotten rather than compared against.
        let bump = |w: &mut Wait| {
            w.last_event = now;
            w.sig = None;
        };
        for d in &self.dirs {
            if path != *d && path.starts_with(d) {
                if let Some(w) = self.items.get_mut(d) {
                    bump(w);
                }
            }
        }
        if is_dir {
            self.dirs.insert(path.clone());
        }
        self.items
            .entry(path)
            .and_modify(bump)
            .or_insert(Wait { first: now, last_event: now, sig: None });
    }

    /// Take out what is ready to be looked up, returning it and anything given
    /// up on after `MAX_WAIT`.
    fn take_ready(
        &mut self,
        now: Instant,
        settle: Duration,
        look: impl Fn(&Path) -> Probe,
    ) -> (Vec<(PathBuf, bool)>, Vec<PathBuf>) {
        let mut ready = Vec::new();
        let mut expired = Vec::new();
        let mut gone = Vec::new();
        for (p, w) in self.items.iter_mut() {
            if now.duration_since(w.first) >= MAX_WAIT {
                expired.push(p.clone());
                continue;
            }
            if now.duration_since(w.last_event) < settle {
                continue;
            }
            let (sig, open, is_dir) = match look(p) {
                Probe::Gone => {
                    gone.push(p.clone());
                    continue;
                }
                // Too big to sum: the settle time is all there is to go on.
                Probe::Dir { sig: None } => {
                    ready.push((p.clone(), true));
                    continue;
                }
                Probe::Dir { sig: Some(sig) } => (sig, true, true),
                Probe::File { sig, open } => (sig, open, false),
            };
            if w.sig == Some(sig) && open {
                ready.push((p.clone(), is_dir));
            } else {
                // Still growing, or not looked at before. Some writers raise
                // no event until they close the file, so a change seen here
                // restarts the settle time.
                if w.sig.is_some() && w.sig != Some(sig) {
                    w.last_event = now;
                }
                w.sig = Some(sig);
            }
        }
        // Either rule below only holds something back for HOLD_MAX.
        let young = |p: &PathBuf| {
            self.items.get(p).map(|w| now.duration_since(w.first) < HOLD_MAX).unwrap_or(false)
        };
        // A folder also waits for anything inside it that is still waiting on
        // its own account - a track held open by its writer, say...
        let going: HashSet<PathBuf> = ready.iter().map(|(p, _)| p.clone()).collect();
        let held: Vec<PathBuf> = ready
            .iter()
            .filter(|(d, is_dir)| {
                *is_dir
                    && young(d)
                    && self.items.keys().any(|q| q != d && q.starts_with(d) && !going.contains(q))
            })
            .map(|(d, _)| d.clone())
            .collect();
        ready.retain(|(p, _)| !held.contains(p));
        // ...and whatever is inside a folder that is still waiting goes out
        // with it, not ahead of it. Otherwise the track goes first and the
        // folder's walk looks it up a second time a moment later. Whatever
        // is held here is ready again on the next look, so once the folder
        // settles they all leave together.
        let going: HashSet<PathBuf> = ready.iter().map(|(p, _)| p.clone()).collect();
        ready.retain(|(p, _)| {
            !young(p)
                || !self.dirs.iter().any(|d| d != p && p.starts_with(d) && !going.contains(d))
        });
        for p in ready.iter().map(|(p, _)| p).chain(&expired).chain(&gone) {
            self.items.remove(p);
            self.dirs.remove(p);
        }
        (ready, expired)
    }
}

/// Drop anything that sits inside a folder that is also being scanned - the
/// walk of the folder covers it.
fn collapse(mut ready: Vec<(PathBuf, bool)>) -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = ready.iter().filter(|(_, d)| *d).map(|(p, _)| p.clone()).collect();
    ready.retain(|(p, _)| !dirs.iter().any(|d| p != d && p.starts_with(d)));
    let mut out: Vec<PathBuf> = ready.into_iter().map(|(p, _)| p).collect();
    out.sort();
    out
}

/// Paths as they can go into the batch file, one per line, in UTF-8. A Linux
/// file name may hold a line break or bytes that are not UTF-8; written out
/// as-is it would arrive as a path that does not exist (or two). Such a path
/// is handed over as the nearest folder above it that can be written - the
/// walk of that folder reads the name straight from the disk.
fn listable(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = paths
        .into_iter()
        .filter_map(|p| {
            let mut q: &Path = &p;
            loop {
                match q.to_str() {
                    Some(s) if !s.contains(['\n', '\r']) => return Some(q.to_path_buf()),
                    _ => q = q.parent()?,
                }
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// the monitor's own thread
// ---------------------------------------------------------------------------

struct Batch {
    child: std::process::Child,
    list: PathBuf,
    count: usize,
}

struct Coordinator {
    settings: Settings,
    log: Log,
    tx: Sender<Msg>,
    gen: u64,
    watchers: Vec<Arc<AtomicBool>>,
    folders: Vec<Folder>,
    /// Per folder: its watcher lost its handle, so changes may have been
    /// missed. Logged once, rather than every retry.
    lost: Vec<bool>,
    pending: Pending,
    batch: Option<Batch>,
    last_batch: Option<(chrono::DateTime<chrono::Local>, usize)>,
    full_child: Option<std::process::Child>,
    full_launched: Option<Instant>,
    next_full: Option<chrono::DateTime<chrono::Utc>>,
    db: Option<Store>,
    /// `monitor.lock`, held while this is the active monitor.
    lock: Option<std::fs::File>,
    last_lock_try: Instant,
    status: Arc<Mutex<Status>>,
}

/// How often a waiting monitor tries for the lock again.
const LEAD_RETRY: Duration = Duration::from_secs(30);

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl Coordinator {
    fn run(mut self, rx: Receiver<Msg>, first: Settings) {
        self.try_lead();
        if self.lock.is_none() {
            self.log.info(
                "monitor: another Lyricarr is already watching the music folders - leaving it to that one",
            );
        }
        self.apply(first, true);
        let mut last_tick = Instant::now();
        let mut last_schedule = Instant::now() - SCHEDULE_EVERY;
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    self.stop_watchers();
                    return;
                }
                Ok(Msg::Configure(s)) => {
                    self.apply(*s, false);
                    last_schedule = Instant::now() - SCHEDULE_EVERY;
                    self.publish();
                }
                Ok(Msg::CheckNow) => last_schedule = Instant::now() - SCHEDULE_EVERY,
                Ok(m) => self.on_watcher(m),
                Err(RecvTimeoutError::Timeout) => {}
            }
            // Time-gated rather than per message: a burst of a thousand
            // events should cost a thousand map updates, not a thousand passes
            // over the waiting list.
            if last_tick.elapsed() >= Duration::from_secs(1) {
                last_tick = Instant::now();
                self.tick();
                if last_schedule.elapsed() >= SCHEDULE_EVERY {
                    last_schedule = Instant::now();
                    self.schedule();
                }
                self.publish();
            }
        }
    }

    fn data_dir(&self) -> PathBuf {
        Path::new(&self.settings.cfg.run.database)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(Config::here)
    }

    /// Try to become the active monitor.
    fn try_lead(&mut self) -> bool {
        self.last_lock_try = Instant::now();
        if self.lock.is_none() {
            self.lock = crate::os::exclusive_lock(&self.data_dir().join("monitor.lock"));
        }
        self.lock.is_some()
    }

    fn apply(&mut self, new: Settings, first: bool) {
        let restart = first
            || new.watch != self.settings.watch
            || new.cfg.library.roots != self.settings.cfg.library.roots
            || new.cfg.library.skip_dir_names != self.settings.cfg.library.skip_dir_names;
        self.settings = new;
        if !restart {
            return;
        }
        self.stop_watchers();
        self.pending.clear();
        self.folders.clear();
        if self.settings.watch && self.lock.is_some() {
            self.start_watchers();
        } else if !first && !self.settings.watch {
            self.log.info("monitor: stopped watching the music folders");
        }
    }

    fn start_watchers(&mut self) {
        self.gen += 1;
        let roots = self.settings.cfg.library.roots.clone();
        self.folders = roots
            .iter()
            .map(|r| Folder { path: r.clone(), state: FolderState::Starting })
            .collect();
        self.lost = vec![false; roots.len()];
        // Lower-cased, for the Linux watcher: every folder costs an inotify
        // watch there, so the ones that would be ignored anyway are skipped.
        let skip: Vec<String> =
            self.settings.cfg.library.skip_dir_names.iter().map(|s| s.to_lowercase()).collect();
        for (i, root) in roots.into_iter().enumerate() {
            let stop = Arc::new(AtomicBool::new(false));
            let (tx, gen, flag, skip) = (self.tx.clone(), self.gen, stop.clone(), skip.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("watch {i}"))
                .spawn(move || platform::watch_root(PathBuf::from(root), i, gen, tx, flag, skip));
            if spawned.is_ok() {
                self.watchers.push(stop);
            }
        }
        if !self.folders.is_empty() {
            self.log.info(&format!(
                "monitor: watching {} for new music",
                plural(self.folders.len(), "folder", "folders")
            ));
        }
    }

    fn stop_watchers(&mut self) {
        // Each watcher notices within half a second and closes its handle.
        for s in self.watchers.drain(..) {
            s.store(true, Ordering::Relaxed);
        }
    }

    fn on_watcher(&mut self, m: Msg) {
        match m {
            Msg::Changed { gen, path, appeared } if gen == self.gen => {
                if interesting(&self.settings.cfg, &path, appeared) {
                    let is_dir = !self.settings.cfg.is_audio(&path);
                    self.pending.note(path, is_dir, Instant::now());
                }
            }
            Msg::Missed { gen, root, why } if gen == self.gen => {
                if let Some(f) = self.folders.get(root) {
                    self.log.info(&format!(
                        "monitor: {} under {} - checking the whole folder",
                        why, f.path
                    ));
                    self.pending.note(PathBuf::from(&f.path), true, Instant::now());
                }
            }
            Msg::Released { gen, root } if gen == self.gen => {
                if let Some(f) = self.folders.get(root) {
                    self.log.info(&format!(
                        "monitor: let go of {} so the drive can be ejected - watching it again \
                         when it is back",
                        f.path
                    ));
                }
            }
            Msg::State { gen, root, state } if gen == self.gen => {
                let Some(f) = self.folders.get_mut(root) else { return };
                if f.state == state {
                    return;
                }
                let before = f.state;
                f.state = state;
                match state {
                    FolderState::Unreachable => {
                        if before == FolderState::Watching {
                            self.lost[root] = true;
                        }
                        self.log.info(&format!(
                            "monitor: cannot watch {} right now (not reachable) - will keep trying",
                            f.path
                        ));
                    }
                    FolderState::Watching | FolderState::Partial
                        if before == FolderState::Unreachable =>
                    {
                        self.log.info(&format!("monitor: watching {} again", f.path));
                    }
                    _ => {}
                }
                if state == FolderState::Partial && before != FolderState::Partial {
                    self.log.info(&format!(
                        "monitor: ran out of inotify watches part-way through {} - new music in \
                         the folders left over waits for the next full scan. Raise the limit \
                         with: sudo sysctl fs.inotify.max_user_watches=524288 (and put it in \
                         /etc/sysctl.d/ to keep it)",
                        f.path
                    ));
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        // Waiting for the active monitor to go away?
        if self.lock.is_none() && self.last_lock_try.elapsed() >= LEAD_RETRY && self.try_lead() {
            self.log.info("monitor: the other Lyricarr has stopped watching - taking over");
            if self.settings.watch {
                self.start_watchers();
            }
        }
        // A batch that has finished.
        if let Some(b) = self.batch.as_mut() {
            if !matches!(b.child.try_wait(), Ok(None)) {
                let b = self.batch.take().unwrap();
                let _ = std::fs::remove_file(&b.list);
                self.last_batch = Some((chrono::Local::now(), b.count));
            }
        }
        // One batch at a time. What arrives meanwhile keeps waiting - it has
        // usually settled by the time this one is done, and goes straight out.
        if self.batch.is_some() || self.pending.len() == 0 {
            return;
        }
        let (ready, expired) = self.pending.take_ready(Instant::now(), self.settings.settle, probe);
        for p in expired {
            self.log.info(&format!(
                "monitor: gave up waiting for {} - it was still changing or in use after an hour",
                p.display()
            ));
        }
        let paths = collapse(ready);
        if !paths.is_empty() {
            self.launch(paths);
        }
    }

    /// Hand a batch to a `lyricarr scan` of its own, as the hook does. A
    /// separate process means a lookup that hangs or crashes cannot take the
    /// tray down with it, and it works through the same database rules as
    /// every other scan.
    fn launch(&mut self, paths: Vec<PathBuf>) {
        let paths = listable(paths);
        if paths.is_empty() {
            return;
        }
        // Through a file rather than the command line: a whole discography
        // dropped in at once would not fit in Windows' 32k characters.
        let list = self.data_dir().join("monitor-batch.txt");
        let body: String = paths.iter().map(|p| format!("{}\n", p.display())).collect();
        if let Err(e) = std::fs::write(&list, body) {
            self.log.info(&format!("monitor: could not write {}: {e}", list.display()));
            return;
        }
        self.log.info(&format!(
            "monitor: new music - {} ready to look up",
            plural(paths.len(), "item", "items")
        ));
        match crate::os::lyricarr()
            .args(["scan", "--quiet", "--paths-from"])
            .arg(&list)
            .spawn()
        {
            Ok(child) => self.batch = Some(Batch { child, list, count: paths.len() }),
            Err(e) => {
                self.log.info(&format!("monitor: could not start a lookup: {e}"));
                let _ = std::fs::remove_file(&list);
            }
        }
    }

    /// The automatic full scan.
    fn schedule(&mut self) {
        if let Some(c) = self.full_child.as_mut() {
            if !matches!(c.try_wait(), Ok(None)) {
                self.full_child = None;
            }
        }
        let hours = self.settings.auto_scan_hours;
        if hours == 0 || self.settings.cfg.library.roots.is_empty() {
            self.next_full = None;
            return;
        }
        if self.db.is_none() {
            self.db = Store::open_readonly(Path::new(&self.settings.cfg.run.database)).ok();
        }
        let meta = |k: &str| self.db.as_ref().and_then(|d| d.get_meta(k)).and_then(|s| store::parse_iso(&s));
        let live = meta("scan_heartbeat")
            .map(|t| (chrono::Utc::now() - t).num_seconds() < 60)
            .unwrap_or(false);
        // From whichever full scan is most recent, started or finished. An
        // install from before this setting existed has only last_scan.
        let last = [meta("last_full_scan"), meta("last_full_scan_started")]
            .into_iter()
            .flatten()
            .max()
            .or_else(|| meta("last_scan"));
        let now = chrono::Utc::now();
        // Never scanned at all: due now.
        let due = last.map(|t| t + chrono::Duration::hours(hours as i64)).unwrap_or(now);
        self.next_full = Some(due);

        // The schedule belongs to whichever monitor is active.
        if self.lock.is_none() || live || self.full_child.is_some() || due > now {
            return;
        }
        if self.full_launched.map(|t| t.elapsed() < RELAUNCH_GAP).unwrap_or(false) {
            return;
        }
        self.full_launched = Some(Instant::now());
        self.log.info(&format!(
            "monitor: starting the automatic full scan (every {})",
            plural(hours as usize, "hour", "hours")
        ));
        match crate::os::lyricarr().args(["scan", "--quiet"]).spawn() {
            Ok(c) => self.full_child = Some(c),
            Err(e) => self.log.info(&format!("monitor: could not start the full scan: {e}")),
        }
    }

    fn publish(&self) {
        if let Ok(mut s) = self.status.lock() {
            s.watch = self.settings.watch;
            s.folders = self.folders.clone();
            s.waiting = self.pending.len();
            s.checking = self.batch.as_ref().map(|b| b.count).unwrap_or(0);
            s.last_batch = self.last_batch;
            s.auto_scan_hours = self.settings.auto_scan_hours;
            s.next_full_scan = self.next_full;
            s.passive = self.lock.is_none();
        }
    }
}

// ---------------------------------------------------------------------------
// ReadDirectoryChangesW
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::{FolderState, Msg};
    use std::cell::Cell;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_NOTIFY_ENUM_DIR, HANDLE, HWND, INVALID_HANDLE_VALUE,
        LPARAM, LRESULT, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
        FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
        FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MsgWaitForMultipleObjects,
        PeekMessageW, RegisterClassW, RegisterDeviceNotificationW, TranslateMessage,
        UnregisterDeviceNotification, DBT_DEVICEQUERYREMOVE, DBT_DEVTYP_HANDLE,
        DEVICE_NOTIFY_WINDOW_HANDLE, DEV_BROADCAST_HANDLE, HDEVNOTIFY, HWND_MESSAGE, MSG,
        PM_REMOVE, QS_ALLINPUT, WM_DEVICECHANGE, WNDCLASSW,
    };

    /// 64 KB is the most ReadDirectoryChangesW will accept over a network
    /// share, and plenty locally: it holds several hundred notifications.
    const BUF: usize = 64 * 1024;

    const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE;

    enum End {
        Stopped,
        Lost,
        /// Windows asked to eject the drive, and the handle was let go - by
        /// the window procedure, so it is already closed.
        Released,
    }

    /// Sleep for up to `d`, waking early if asked to stop.
    fn nap(d: Duration, stop: &AtomicBool) {
        let until = Instant::now() + d;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn open(root: &Path) -> Option<HANDLE> {
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // Every share mode, so watching never stops anyone from renaming,
        // moving or deleting anything in the library.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            None
        } else {
            Some(h)
        }
    }

    // ---- letting go when a drive is ejected -------------------------------
    //
    // A directory handle held open is exactly what makes Windows refuse to
    // eject a USB drive: "This device is currently in use". Before ejecting,
    // Windows asks everyone who registered for a handle on the drive
    // (DBT_DEVICEQUERYREMOVE), and closing the handle before answering is
    // what lets the eject go ahead. The question comes as a window message,
    // so each watcher thread has a message-only window of its own and pumps
    // its messages while it waits for changes.

    thread_local! {
        /// The handle, and the read outstanding on it, that the window
        /// procedure lets go of. Null whenever no read is outstanding.
        static HELD: Cell<(HANDLE, *const OVERLAPPED)> = const { Cell::new((null_mut(), null())) };
        /// Set by the window procedure once it has.
        static LET_GO: Cell<bool> = const { Cell::new(false) };
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // Each watcher's window is registered for its own handle only, so a
        // removal query here is about that handle.
        if msg == WM_DEVICECHANGE && wparam == DBT_DEVICEQUERYREMOVE as usize {
            let (h, ov) = HELD.with(|c| c.replace((null_mut(), null())));
            if !h.is_null() {
                if !ov.is_null() {
                    cancel(h, &*ov);
                }
                CloseHandle(h);
                LET_GO.with(|c| c.set(true));
            }
            return 1; // TRUE: nothing here stands in the way
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn class_name() -> &'static [u16] {
        static NAME: OnceLock<Vec<u16>> = OnceLock::new();
        NAME.get_or_init(|| {
            let name: Vec<u16> =
                "LyricarrWatchEject".encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                let mut wc: WNDCLASSW = std::mem::zeroed();
                wc.lpfnWndProc = Some(wndproc);
                wc.hInstance = GetModuleHandleW(null());
                wc.lpszClassName = name.as_ptr();
                // A failure shows up as CreateWindowExW failing, below.
                RegisterClassW(&wc);
            }
            name
        })
    }

    /// A message-only window registered to hear about `h` being ejected. None
    /// when Windows will not provide one, and the drive then cannot be
    /// ejected while it is watched - which is how it was before this existed.
    fn listen(h: HANDLE) -> Option<(HWND, HDEVNOTIFY)> {
        unsafe {
            let hwnd = CreateWindowExW(
                0,
                class_name().as_ptr(),
                null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            if hwnd.is_null() {
                return None;
            }
            let mut f: DEV_BROADCAST_HANDLE = std::mem::zeroed();
            f.dbch_size = std::mem::size_of::<DEV_BROADCAST_HANDLE>() as u32;
            f.dbch_devicetype = DBT_DEVTYP_HANDLE;
            f.dbch_handle = h;
            let note = RegisterDeviceNotificationW(
                hwnd,
                (&f as *const DEV_BROADCAST_HANDLE).cast(),
                DEVICE_NOTIFY_WINDOW_HANDLE,
            );
            if note.is_null() {
                DestroyWindow(hwnd);
                return None;
            }
            Some((hwnd, note))
        }
    }

    fn unlisten(ears: Option<(HWND, HDEVNOTIFY)>) {
        if let Some((hwnd, note)) = ears {
            unsafe {
                UnregisterDeviceNotification(note);
                DestroyWindow(hwnd);
            }
        }
    }

    /// Watch one music folder until told to stop, reopening it whenever it
    /// goes away. Runs on a thread of its own.
    /// `_skip` is for the Linux watcher; one handle here covers the tree.
    pub fn watch_root(
        root: PathBuf,
        idx: usize,
        gen: u64,
        tx: Sender<Msg>,
        stop: Arc<AtomicBool>,
        _skip: Vec<String>,
    ) {
        let say = |state| {
            let _ = tx.send(Msg::State { gen, root: idx, state });
        };
        let mut lost = false;
        while !stop.load(Ordering::Relaxed) {
            let Some(h) = open(&root) else {
                say(FolderState::Unreachable);
                nap(Duration::from_secs(30), &stop);
                continue;
            };
            // Said once the first read is queued, not before: Windows only
            // collects changes from that first ReadDirectoryChangesW on, so
            // "watching" any earlier would be claiming what is not yet so.
            let ready = || {
                say(FolderState::Watching);
                if lost {
                    // It went away while being watched, so whatever changed
                    // in the meantime was never reported. (No need to clear
                    // `lost`: the pump ends either the thread or with it set
                    // again.)
                    let _ = tx.send(Msg::Missed { gen, root: idx, why: "lost touch for a while" });
                }
            };
            let end = pump(h, &root, idx, gen, &tx, &stop, ready);
            if !matches!(end, End::Released) {
                unsafe { CloseHandle(h) };
            }
            match end {
                End::Stopped => return,
                End::Lost => {
                    lost = true;
                    say(FolderState::Unreachable);
                    nap(Duration::from_secs(10), &stop);
                }
                End::Released => {
                    lost = true;
                    let _ = tx.send(Msg::Released { gen, root: idx });
                    say(FolderState::Unreachable);
                    // Long enough for the eject to finish. If it was called
                    // off after all, the folder is simply opened again.
                    nap(Duration::from_secs(30), &stop);
                }
            }
        }
    }

    /// Cancel an outstanding read and wait for the cancellation to land. The
    /// buffer and OVERLAPPED belong to the kernel until it has: returning
    /// first would free memory it is about to write into.
    unsafe fn cancel(h: HANDLE, ov: &OVERLAPPED) {
        CancelIoEx(h, ov);
        let mut n = 0u32;
        GetOverlappedResult(h, ov, &mut n, 1);
    }

    fn pump(
        h: HANDLE,
        root: &Path,
        idx: usize,
        gen: u64,
        tx: &Sender<Msg>,
        stop: &AtomicBool,
        ready: impl FnOnce(),
    ) -> End {
        let mut ready = Some(ready);
        // ReadDirectoryChangesW wants a DWORD-aligned buffer; u64s give that.
        let mut buf = vec![0u64; BUF / 8];
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return End::Lost;
        }
        let ears = listen(h);
        LET_GO.with(|c| c.set(false));
        let forget = || HELD.with(|c| c.set((null_mut(), null())));
        let end = 'outer: loop {
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = event;
            unsafe { ResetEvent(event) };
            let ok = unsafe {
                ReadDirectoryChangesW(
                    h,
                    buf.as_mut_ptr().cast(),
                    BUF as u32,
                    1, // the whole tree
                    FILTER,
                    null_mut(),
                    &mut ov,
                    None,
                )
            };
            if ok == 0 {
                break End::Lost;
            }
            if let Some(f) = ready.take() {
                f();
            }
            HELD.with(|c| c.set((h, &ov as *const OVERLAPPED)));
            // Waiting in half-second slices is what lets the thread notice
            // it has been asked to stop; waiting on messages as well is what
            // lets the window hear about an eject.
            loop {
                let w = unsafe { MsgWaitForMultipleObjects(1, &event, 0, 500, QS_ALLINPUT) };
                if w == WAIT_OBJECT_0 {
                    break;
                }
                if w == WAIT_OBJECT_0 + 1 {
                    unsafe {
                        let mut msg: MSG = std::mem::zeroed();
                        while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                    if LET_GO.with(|c| c.get()) {
                        // The read was cancelled and the handle closed by the
                        // window procedure; neither may be touched again.
                        break 'outer End::Released;
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    forget();
                    unsafe { cancel(h, &ov) };
                    break 'outer End::Stopped;
                }
                if w != WAIT_TIMEOUT && w != WAIT_OBJECT_0 + 1 {
                    forget();
                    unsafe { cancel(h, &ov) };
                    break 'outer End::Lost;
                }
            }
            forget();
            let mut n = 0u32;
            if unsafe { GetOverlappedResult(h, &ov, &mut n, 0) } == 0 {
                if unsafe { GetLastError() } == ERROR_NOTIFY_ENUM_DIR {
                    let _ = tx.send(Msg::Missed { gen, root: idx, why: "too many changes at once" });
                    continue;
                }
                // The share dropped, the drive was pulled, the folder was
                // deleted or renamed.
                break End::Lost;
            }
            if n == 0 {
                // More arrived than the buffer holds; the details are lost.
                let _ = tx.send(Msg::Missed { gen, root: idx, why: "too many changes at once" });
                continue;
            }
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
            read_notifications(bytes, root, gen, tx);
        };
        forget();
        unlisten(ears);
        unsafe { CloseHandle(event) };
        end
    }

    /// Walk the FILE_NOTIFY_INFORMATION records in one completed read.
    fn read_notifications(bytes: &[u8], root: &Path, gen: u64, tx: &Sender<Msg>) {
        for (path, appeared) in parse_notifications(bytes, root) {
            let _ = tx.send(Msg::Changed { gen, path, appeared });
        }
    }

    /// The changed paths in a buffer of FILE_NOTIFY_INFORMATION records, and
    /// for each whether it appeared (added, or renamed to) or was modified.
    ///
    /// Read field by field out of the bytes at their documented offsets -
    /// NextEntryOffset, Action, FileNameLength, FileName at 0, 4, 8, 12 -
    /// rather than by casting a pointer to the struct. Windows pads each
    /// record to four bytes, but Wine does not, and a cast onto an unpadded
    /// record is a misaligned read: in a debug build that aborted the whole
    /// program from the watcher thread. This way needs no alignment, no
    /// `unsafe`, and stops at the first record that does not fit.
    pub(super) fn parse_notifications(bytes: &[u8], root: &Path) -> Vec<(PathBuf, bool)> {
        let u32_at = |at: usize| -> Option<u32> {
            bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let mut out = Vec::new();
        let mut off = 0usize;
        while let (Some(next), Some(action), Some(len)) =
            (u32_at(off), u32_at(off + 4), u32_at(off + 8))
        {
            let Some(name) = bytes.get(off + 12..off + 12 + len as usize) else { break };
            let wide: Vec<u16> =
                name.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let appeared = match action {
                FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => Some(true),
                FILE_ACTION_MODIFIED => Some(false),
                // Removed, and the old half of a rename: nothing to look up.
                _ => None,
            };
            if let Some(appeared) = appeared {
                out.push((root.join(PathBuf::from(OsString::from_wide(&wide))), appeared));
            }
            // Zero ends the list. Anything shorter than a record header would
            // be a corrupt buffer, and following it could loop forever.
            if next == 0 || (next as usize) < 12 {
                break;
            }
            off += next as usize;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// inotify
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use super::{FolderState, Msg};
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::io::RawFd;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Created, moved in, or finished being written. IN_CLOSE_WRITE is the
    /// best "it has finished copying" signal any system offers; IN_MODIFY
    /// would be one event per write() - thousands for a single FLAC - and the
    /// settle checks catch growth without it.
    const MASK: u32 = libc::IN_CREATE
        | libc::IN_MOVED_TO
        | libc::IN_CLOSE_WRITE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR
        | libc::IN_DONT_FOLLOW
        | libc::IN_EXCL_UNLINK;

    enum End {
        Stopped,
        Lost,
    }

    /// Sleep for up to `d`, waking early if asked to stop.
    fn nap(d: Duration, stop: &AtomicBool) {
        let until = Instant::now() + d;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// One inotify instance and a watch on every folder of one music folder.
    struct Watches {
        fd: RawFd,
        /// Watch descriptor -> the folder it watches.
        dirs: HashMap<i32, PathBuf>,
        root_wd: i32,
        /// The kernel's limit on watches ran out somewhere in the tree.
        full: bool,
        skip: Vec<String>,
    }

    impl Drop for Watches {
        fn drop(&mut self) {
            // Closing the descriptor drops every watch on it at once.
            unsafe { libc::close(self.fd) };
        }
    }

    impl Watches {
        fn new(skip: Vec<String>) -> Option<Self> {
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            (fd >= 0).then_some(Self { fd, dirs: HashMap::new(), root_wd: -1, full: false, skip })
        }

        fn skipped(&self, name: &OsStr) -> bool {
            let lower = name.to_string_lossy().to_lowercase();
            lower.starts_with(".trash-") || self.skip.contains(&lower)
        }

        fn add(&mut self, dir: &Path) -> Option<i32> {
            let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
            let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
            if wd < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOSPC) {
                    self.full = true;
                }
                return None;
            }
            // The same folder reached under a new name (a rename inside the
            // library) gets the same descriptor back: this updates its path.
            self.dirs.insert(wd, dir.to_path_buf());
            Some(wd)
        }

        /// Watch `top` and every folder under it. Symlinks are not followed:
        /// a link back up the tree would otherwise never end.
        fn add_tree(&mut self, top: &Path, stop: &AtomicBool) {
            let mut stack = vec![top.to_path_buf()];
            while let Some(d) = stack.pop() {
                if self.full || stop.load(Ordering::Relaxed) {
                    return;
                }
                if self.add(&d).is_none() {
                    continue;
                }
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    if is_dir && !self.skipped(&e.file_name()) {
                        stack.push(e.path());
                    }
                }
            }
        }
    }

    /// Watch one music folder until told to stop, reopening it whenever it
    /// goes away. Runs on a thread of its own.
    pub fn watch_root(
        root: PathBuf,
        idx: usize,
        gen: u64,
        tx: Sender<Msg>,
        stop: Arc<AtomicBool>,
        skip: Vec<String>,
    ) {
        let say = |state| {
            let _ = tx.send(Msg::State { gen, root: idx, state });
        };
        let mut lost = false;
        while !stop.load(Ordering::Relaxed) {
            if !root.is_dir() {
                say(FolderState::Unreachable);
                nap(Duration::from_secs(30), &stop);
                continue;
            }
            let Some(mut w) = Watches::new(skip.clone()) else {
                // Out of inotify instances (fs.inotify.max_user_instances).
                say(FolderState::Unreachable);
                nap(Duration::from_secs(30), &stop);
                continue;
            };
            match w.add(&root) {
                Some(wd) => w.root_wd = wd,
                None => {
                    say(FolderState::Unreachable);
                    nap(Duration::from_secs(30), &stop);
                    continue;
                }
            }
            // The root is watched from here on, so nothing that arrives while
            // the rest of the tree is being added goes unseen at the top.
            let rest: Vec<PathBuf> = std::fs::read_dir(&root)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .filter(|e| !w.skipped(&e.file_name()))
                        .map(|e| e.path())
                        .collect()
                })
                .unwrap_or_default();
            for d in rest {
                w.add_tree(&d, &stop);
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            say(if w.full { FolderState::Partial } else { FolderState::Watching });
            if lost {
                let _ = tx.send(Msg::Missed { gen, root: idx, why: "lost touch for a while" });
            }
            match pump(&mut w, idx, gen, &tx, &stop) {
                End::Stopped => return,
                End::Lost => {
                    lost = true;
                    say(FolderState::Unreachable);
                    nap(Duration::from_secs(10), &stop);
                }
            }
        }
    }

    fn pump(w: &mut Watches, idx: usize, gen: u64, tx: &Sender<Msg>, stop: &AtomicBool) -> End {
        let mut buf = vec![0u8; 64 * 1024];
        let was_full = w.full;
        loop {
            if stop.load(Ordering::Relaxed) {
                return End::Stopped;
            }
            // Half-second slices, so the thread notices it has been asked to
            // stop.
            let mut pfd = libc::pollfd { fd: w.fd, events: libc::POLLIN, revents: 0 };
            let r = unsafe { libc::poll(&mut pfd, 1, 500) };
            if r < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return End::Lost;
            }
            if r == 0 {
                continue;
            }
            let n = unsafe { libc::read(w.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::EAGAIN) | Some(libc::EINTR) => continue,
                    _ => return End::Lost,
                }
            }
            for ev in parse_events(&buf[..n as usize]) {
                if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                    let _ = tx.send(Msg::Missed { gen, root: idx, why: "too many changes at once" });
                    continue;
                }
                // The music folder itself deleted, moved away or unmounted.
                let gone = libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_UNMOUNT | libc::IN_IGNORED;
                if ev.wd == w.root_wd && ev.mask & gone != 0 {
                    return End::Lost;
                }
                if ev.mask & libc::IN_IGNORED != 0 {
                    w.dirs.remove(&ev.wd);
                    continue;
                }
                let Some(dir) = w.dirs.get(&ev.wd).cloned() else { continue };
                if ev.name.is_empty() {
                    continue;
                }
                let path = dir.join(std::ffi::OsString::from_vec(ev.name));
                let arrived = ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0;
                if arrived && ev.mask & libc::IN_ISDIR != 0 {
                    // A new folder: watch it and everything in it. Files
                    // written into it before its watch existed are still
                    // found - the folder itself is reported, and walked.
                    if !w.skipped(path.file_name().unwrap_or_default()) {
                        w.add_tree(&path, stop);
                    }
                    let _ = tx.send(Msg::Changed { gen, path, appeared: true });
                } else if arrived {
                    let _ = tx.send(Msg::Changed { gen, path, appeared: true });
                } else if ev.mask & libc::IN_CLOSE_WRITE != 0 {
                    let _ = tx.send(Msg::Changed { gen, path, appeared: false });
                }
            }
            if w.full && !was_full {
                let _ = tx.send(Msg::State { gen, root: idx, state: FolderState::Partial });
            }
        }
    }

    #[derive(Debug, PartialEq)]
    pub(super) struct Event {
        pub wd: i32,
        pub mask: u32,
        pub name: Vec<u8>,
    }

    /// The `inotify_event` records in one read: wd, mask, cookie and len at
    /// 0, 4, 8 and 12, then `len` bytes of NUL-padded name. Read field by field
    /// from the bytes, so nothing depends on alignment, and stopped at the
    /// first record that does not fit.
    pub(super) fn parse_events(bytes: &[u8]) -> Vec<Event> {
        let u32_at = |at: usize| -> Option<u32> {
            bytes.get(at..at + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        };
        let mut out = Vec::new();
        let mut off = 0usize;
        while let (Some(wd), Some(mask), Some(len)) = (u32_at(off), u32_at(off + 4), u32_at(off + 12)) {
            let Some(raw) = bytes.get(off + 16..off + 16 + len as usize) else { break };
            let name = raw.iter().take_while(|b| **b != 0).copied().collect();
            out.push(Event { wd: wd as i32, mask, name });
            off += 16 + len as usize;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test path written Windows-style, as a real path on whichever system
    /// runs the test - a backslash is no separator on Linux.
    fn m(s: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(s)
        } else {
            PathBuf::from(s.replace('\\', "/"))
        }
    }

    fn file(size: u64) -> Probe {
        Probe::File { sig: (size, None), open: true }
    }

    fn ready_paths(r: &[(PathBuf, bool)]) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = r.iter().map(|(p, _)| p.clone()).collect();
        v.sort();
        v
    }

    const SETTLE: Duration = Duration::from_secs(10);

    #[test]
    fn a_file_waits_for_quiet_then_one_steady_look() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        p.note(m(r"M:\a\1.flac"), false, t0);

        // Too soon.
        let (r, _) = p.take_ready(t0 + Duration::from_secs(5), SETTLE, |_| file(100));
        assert!(r.is_empty());
        // Quiet long enough, but this is the first look - remember the size.
        let (r, _) = p.take_ready(t0 + Duration::from_secs(10), SETTLE, |_| file(100));
        assert!(r.is_empty());
        // Same size a second later: ready.
        let (r, _) = p.take_ready(t0 + Duration::from_secs(11), SETTLE, |_| file(100));
        assert_eq!(ready_paths(&r), vec![m(r"M:\a\1.flac")]);
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn a_file_that_keeps_growing_is_not_ready() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        p.note(m(r"M:\a\1.flac"), false, t0);
        let _ = p.take_ready(t0 + Duration::from_secs(10), SETTLE, |_| file(100));
        // Grew without any event (some writers only notify on close): the
        // settle time starts again from here.
        let (r, _) = p.take_ready(t0 + Duration::from_secs(11), SETTLE, |_| file(200));
        assert!(r.is_empty());
        let (r, _) = p.take_ready(t0 + Duration::from_secs(15), SETTLE, |_| file(200));
        assert!(r.is_empty(), "the growth should have restarted the settle time");
        let (r, _) = p.take_ready(t0 + Duration::from_secs(21), SETTLE, |_| file(200));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn a_file_held_open_by_its_writer_waits() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        p.note(m(r"M:\a\1.flac"), false, t0);
        let locked = |_: &Path| Probe::File { sig: (100, None), open: false };
        for s in 10..20 {
            let (r, _) = p.take_ready(t0 + Duration::from_secs(s), SETTLE, locked);
            assert!(r.is_empty());
        }
        let (r, _) = p.take_ready(t0 + Duration::from_secs(20), SETTLE, |_| file(100));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn a_folder_waits_while_anything_inside_it_is_changing() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        let album = m(r"M:\Artist\Album");
        p.note(album.clone(), true, t0);
        // Its tracks are still being copied in, one every few seconds.
        for s in [3, 6, 9, 12] {
            p.note(album.join(format!("{s}.flac")), false, t0 + Duration::from_secs(s));
        }
        let look = |q: &Path| {
            if q.extension().is_some() { file(100) } else { Probe::Dir { sig: Some((400, None)) } }
        };
        let (r, _) = p.take_ready(t0 + Duration::from_secs(15), SETTLE, look);
        assert!(
            !r.iter().any(|(q, _)| *q == album),
            "the folder went out while its files were still arriving"
        );
        // The last track landed at 12s, so the folder is quiet from 22s; it
        // goes out on the next look that finds it the same size.
        let (r, _) = p.take_ready(t0 + Duration::from_secs(21), SETTLE, look);
        assert!(!r.iter().any(|(q, _)| *q == album));
        let (r, _) = p.take_ready(t0 + Duration::from_secs(22), SETTLE, look);
        assert!(!r.iter().any(|(q, _)| *q == album), "released on its first look");
        let (r, _) = p.take_ready(t0 + Duration::from_secs(23), SETTLE, look);
        assert!(r.iter().any(|(q, d)| *q == album && *d));
    }

    #[test]
    fn a_folder_still_growing_without_events_waits() {
        // The case that event timing alone got wrong: a track inside the
        // folder grows, and Windows says nothing about it until it closes.
        let t0 = Instant::now();
        let mut p = Pending::default();
        let album = m(r"M:\Artist\Album");
        p.note(album.clone(), true, t0);
        let dir = |n: u64| move |_: &Path| Probe::Dir { sig: Some((n, None)) };
        let _ = p.take_ready(t0 + Duration::from_secs(10), SETTLE, dir(100));
        let (r, _) = p.take_ready(t0 + Duration::from_secs(11), SETTLE, dir(500));
        assert!(r.is_empty());
        // Growth seen at 11s restarts the settle time, so 20s is too soon...
        let (r, _) = p.take_ready(t0 + Duration::from_secs(20), SETTLE, dir(900));
        assert!(r.is_empty(), "the growth should have restarted the settle time");
        // ...and at 21s it has grown again: restart once more.
        let (r, _) = p.take_ready(t0 + Duration::from_secs(21), SETTLE, dir(900));
        assert!(r.is_empty());
        let (r, _) = p.take_ready(t0 + Duration::from_secs(30), SETTLE, dir(900));
        assert!(r.is_empty());
        // Ten quiet seconds after the last growth, and the same size: done.
        let (r, _) = p.take_ready(t0 + Duration::from_secs(31), SETTLE, dir(900));
        assert_eq!(ready_paths(&r), vec![album]);
    }

    #[test]
    fn a_track_waits_for_the_new_folder_it_arrived_in() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        let artist = m(r"M:\Artist");
        let album = artist.join("Album");
        let track = album.join("1.flac");
        p.note(artist.clone(), true, t0);
        p.note(album.clone(), true, t0);
        p.note(track.clone(), false, t0);
        // The track is done and quiet, but the artist folder is still
        // changing - another album is on its way in.
        let mut sizes = 100u64;
        for s in [10u64, 11, 12, 13] {
            sizes += 50;
            let n = sizes;
            let look = move |q: &Path| {
                if q.extension().is_some() {
                    file(100)
                } else if q.ends_with("Artist") {
                    Probe::Dir { sig: Some((n, None)) }
                } else {
                    Probe::Dir { sig: Some((100, None)) }
                }
            };
            let (r, _) = p.take_ready(t0 + Duration::from_secs(s), SETTLE, look);
            assert!(r.is_empty(), "something left ahead of its folder at {s}s: {r:?}");
        }
        assert_eq!(p.len(), 3);
        // Everything steady: one walk of the artist folder covers the lot.
        let steady = |q: &Path| {
            if q.extension().is_some() { file(100) } else { Probe::Dir { sig: Some((300, None)) } }
        };
        // The artist folder last grew at 11s; at 23s it has grown since, so
        // its settle time starts again from there...
        let (r, _) = p.take_ready(t0 + Duration::from_secs(23), SETTLE, steady);
        assert!(r.is_empty());
        let (r, _) = p.take_ready(t0 + Duration::from_secs(32), SETTLE, steady);
        assert!(r.is_empty());
        // ...and ten seconds on, still the same size: everything goes, as one.
        let (r, _) = p.take_ready(t0 + Duration::from_secs(33), SETTLE, steady);
        assert_eq!(collapse(r), vec![artist]);
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn a_locked_track_holds_its_folder_back_only_so_long() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        let album = m(r"M:\Artist\Album");
        p.note(album.clone(), true, t0);
        p.note(album.join("stuck.flac"), false, t0);
        let look = |q: &Path| {
            if q.extension().is_some() {
                Probe::File { sig: (100, None), open: false }
            } else {
                Probe::Dir { sig: Some((100, None)) }
            }
        };
        let _ = p.take_ready(t0 + SETTLE, SETTLE, look);
        let (r, _) = p.take_ready(t0 + HOLD_MAX - Duration::from_secs(1), SETTLE, look);
        assert!(r.is_empty());
        let (r, _) = p.take_ready(t0 + HOLD_MAX, SETTLE, look);
        assert_eq!(ready_paths(&r), vec![album], "the rest of the album should go without it");
        assert_eq!(p.len(), 1, "the locked track keeps waiting on its own");
    }

    #[test]
    fn a_folder_that_never_settles_does_not_hold_a_finished_track_forever() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        let dl = m(r"M:\Downloads in progress");
        let done = dl.join("finished.flac");
        p.note(dl.clone(), true, t0);
        p.note(done.clone(), false, t0);
        // Something else in the folder changes every few seconds, forever.
        let mut s = 10;
        while t0 + Duration::from_secs(s) < t0 + HOLD_MAX {
            p.note(dl.join("busy.part"), false, t0 + Duration::from_secs(s));
            let (r, _) = p.take_ready(t0 + Duration::from_secs(s), SETTLE, |q: &Path| {
                if q.extension().is_some() { file(100) } else { Probe::Dir { sig: None } }
            });
            assert!(r.iter().all(|(q, _)| *q != done), "went out early at {s}s");
            s += 5;
        }
        let look = |q: &Path| if q.extension().is_some() { file(100) } else { Probe::Dir { sig: None } };
        let (r, _) = p.take_ready(t0 + HOLD_MAX + Duration::from_secs(1), SETTLE, look);
        assert!(r.iter().any(|(q, _)| *q == done), "the finished track should have gone by now");
    }

    #[test]
    fn a_folder_waits_for_a_file_inside_that_is_still_locked() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        let album = m(r"M:\Artist\Album");
        let track = album.join("1.flac");
        p.note(album.clone(), true, t0);
        p.note(track.clone(), false, t0);
        let look = |q: &Path| {
            if q.extension().is_some() {
                Probe::File { sig: (100, None), open: false }
            } else {
                Probe::Dir { sig: Some((100, None)) }
            }
        };
        for s in 10..15 {
            let (r, _) = p.take_ready(t0 + Duration::from_secs(s), SETTLE, look);
            assert!(r.is_empty(), "went out at {s}s with a track still being written");
        }
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn folder_sig_follows_what_is_inside() {
        let dir = std::env::temp_dir().join(format!("lyr-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("CD2")).unwrap();
        let empty = folder_sig(&dir).unwrap();
        std::fs::write(dir.join("CD2").join("1.flac"), b"abc").unwrap();
        let one = folder_sig(&dir).unwrap();
        assert_ne!(empty, one, "a file in a subfolder should count");
        std::fs::write(dir.join("CD2").join("1.flac"), b"abcdef").unwrap();
        assert_ne!(one, folder_sig(&dir).unwrap(), "growth should count");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whatever_is_gone_is_dropped_quietly() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        p.note(m(r"M:\a\partial.flac"), false, t0);
        let (r, expired) = p.take_ready(t0 + SETTLE, SETTLE, |_| Probe::Gone);
        assert!(r.is_empty() && expired.is_empty());
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn something_stuck_for_an_hour_is_given_up_on() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        p.note(m(r"M:\a\1.flac"), false, t0);
        let locked = |_: &Path| Probe::File { sig: (1, None), open: false };
        let (r, expired) = p.take_ready(t0 + MAX_WAIT, SETTLE, locked);
        assert!(r.is_empty());
        assert_eq!(expired, vec![m(r"M:\a\1.flac")]);
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn files_inside_a_folder_being_scanned_are_left_to_it() {
        let ready = vec![
            (m(r"M:\Artist\Album"), true),
            (m(r"M:\Artist\Album\1.flac"), false),
            (m(r"M:\Artist\Album\CD2\1.flac"), false),
            (m(r"M:\Artist\Album 2\1.flac"), false),
            (m(r"M:\Other\x.mp3"), false),
        ];
        assert_eq!(
            collapse(ready),
            vec![
                m(r"M:\Artist\Album"),
                m(r"M:\Artist\Album 2\1.flac"),
                m(r"M:\Other\x.mp3"),
            ],
            "`Album 2` only shares a prefix of characters with `Album`, not a folder"
        );
    }

    #[cfg(windows)]
    /// One FILE_NOTIFY_INFORMATION record. `pad` rounds it to four bytes as
    /// Windows does; without it, records sit back to back the way Wine
    /// writes them.
    fn record(action: u32, name: &str, last: bool, pad: bool) -> Vec<u8> {
        let wide: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut len = 12 + wide.len();
        if pad {
            len = (len + 3) & !3;
        }
        let mut r = Vec::new();
        r.extend_from_slice(&(if last { 0 } else { len as u32 }).to_le_bytes());
        r.extend_from_slice(&action.to_le_bytes());
        r.extend_from_slice(&(wide.len() as u32).to_le_bytes());
        r.extend_from_slice(&wide);
        r.resize(len, 0);
        r
    }

    #[cfg(windows)]
    #[test]
    fn notifications_parse_padded_or_not() {
        const ADDED: u32 = 1;
        const REMOVED: u32 = 2;
        const MODIFIED: u32 = 3;
        const RENAMED_NEW: u32 = 5;
        let root = Path::new(r"M:\Music");
        for pad in [true, false] {
            let mut buf = Vec::new();
            buf.extend(record(ADDED, "Back On", false, pad));
            buf.extend(record(REMOVED, r"Back On\old.flac", false, pad));
            buf.extend(record(MODIFIED, r"Back On\z.flac", false, pad));
            buf.extend(record(RENAMED_NEW, r"Björk\Homogenic", true, pad));
            // Start the buffer at an odd address too, so nothing can quietly
            // depend on where the allocator happened to put it.
            let mut shifted = vec![0u8];
            shifted.extend(&buf);
            assert_eq!(
                win::parse_notifications(&shifted[1..], root),
                vec![
                    (root.join("Back On"), true),
                    (root.join(r"Back On\z.flac"), false),
                    (root.join(r"Björk\Homogenic"), true),
                ],
                "pad = {pad}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_damaged_notification_buffer_stops_rather_than_crashing() {
        let root = Path::new(r"M:\Music");
        let whole = record(1, "Album", true, true);
        // 12 bytes of header and 10 of name; the last two are only padding.
        let full = 12 + 2 * "Album".len();
        // Cut short anywhere: never a panic, never a half-read name.
        for cut in 0..full {
            let got = win::parse_notifications(&whole[..cut], root);
            assert!(got.is_empty(), "cut at {cut} gave {got:?}");
        }
        for cut in full..=whole.len() {
            assert_eq!(win::parse_notifications(&whole[..cut], root).len(), 1, "cut at {cut}");
        }
        // A next-entry offset that points back into the header would loop.
        let mut bad = record(1, "A", false, true);
        bad[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(win::parse_notifications(&bad, root).len(), 1);
    }

    #[test]
    fn only_audio_and_new_folders_are_interesting() {
        let cfg = Config::default();
        let dir = std::env::temp_dir().join(format!("lyr-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(interesting(&cfg, &dir.join("01 - Song.flac"), false));
        assert!(interesting(&cfg, &dir.join("01 - Song.MP3"), true));
        // Lyricarr's own output, and what it writes on the way there.
        assert!(!interesting(&cfg, &dir.join("01 - Song.lrc"), true));
        assert!(!interesting(&cfg, &dir.join("01 - Song.lrc.lyricarr.tmp"), true));
        assert!(!interesting(&cfg, &dir.join("cover.jpg"), true));
        // A folder that appeared is followed; one that was only modified - as
        // every lyrics file written into it does - is not.
        assert!(interesting(&cfg, &dir, true));
        assert!(!interesting(&cfg, &dir, false));
        // skip_dir_names anywhere up the path.
        assert!(!interesting(&cfg, &dir.join("$RECYCLE.BIN").join("x.flac"), false));
    }

    #[cfg(target_os = "linux")]
    fn inotify_record(wd: i32, mask: u32, name: &str, pad_to: usize) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&wd.to_ne_bytes());
        r.extend_from_slice(&mask.to_ne_bytes());
        r.extend_from_slice(&0u32.to_ne_bytes());
        let mut n = name.as_bytes().to_vec();
        if !n.is_empty() {
            n.push(0);
            n.resize(pad_to.max(n.len()), 0);
        }
        r.extend_from_slice(&(n.len() as u32).to_ne_bytes());
        r.extend_from_slice(&n);
        r
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inotify_events_parse_and_stop_at_damage() {
        let mut buf = inotify_record(1, libc::IN_CREATE, "Album", 16);
        buf.extend(inotify_record(2, libc::IN_CLOSE_WRITE, "01 - Björk.flac", 32));
        buf.extend(inotify_record(3, libc::IN_Q_OVERFLOW, "", 0));
        let got = linux::parse_events(&buf);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].name, b"Album");
        assert_eq!(got[1].name, "01 - Björk.flac".as_bytes());
        assert_eq!(got[2].mask, libc::IN_Q_OVERFLOW);
        // Shifted to an odd address, and cut short anywhere: never a panic.
        let mut shifted = vec![0u8];
        shifted.extend(&buf);
        assert_eq!(linux::parse_events(&shifted[1..]).len(), 3);
        for cut in 0..buf.len() {
            let _ = linux::parse_events(&buf[..cut]);
        }
    }

    /// Running under Wine rather than Windows (or not on Windows at all).
    fn under_wine() -> bool {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
            let ntdll: Vec<u16> = "ntdll.dll".encode_utf16().chain(std::iter::once(0)).collect();
            let m = GetModuleHandleW(ntdll.as_ptr());
            !m.is_null() && GetProcAddress(m, b"wine_get_version\0".as_ptr()).is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The real thing, with the real watcher for this system: watch a
    /// folder, then add an album to it.
    #[test]
    fn the_watcher_reports_new_music_and_new_folders() {
        use std::sync::mpsc::channel;
        let root = std::env::temp_dir().join(format!("lyr-watcher-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Existing")).unwrap();
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (r, s) = (root.clone(), stop.clone());
        let t = std::thread::spawn(move || platform::watch_root(r, 0, 7, tx, s, vec![]));
        let wait_for = |want: &dyn Fn(&Msg) -> bool| {
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                if let Ok(m) = rx.recv_timeout(Duration::from_millis(100)) {
                    if want(&m) {
                        return true;
                    }
                }
            }
            false
        };
        assert!(wait_for(&|m| matches!(m, Msg::State { state: FolderState::Watching, .. })));

        // A track written straight into the music folder...
        std::fs::write(root.join("0.flac"), b"x").unwrap();
        let top = root.join("0.flac");
        assert!(wait_for(&|m| matches!(m, Msg::Changed { path, .. } if *path == top)));

        // ...and into a folder that was there from the start. Not under Wine:
        // its stand-in for ReadDirectoryChangesW only watches the folders it
        // saw being created, where Windows itself watches the whole tree.
        if !under_wine() {
            std::fs::write(root.join("Existing").join("1.flac"), b"x").unwrap();
            let track = root.join("Existing").join("1.flac");
            assert!(wait_for(&|m| matches!(m, Msg::Changed { path, .. } if *path == track)));
        }

        // ...and a new album folder, then a track inside it, which needs the
        // watch added for the new folder.
        std::fs::create_dir_all(root.join("New Album")).unwrap();
        let album = root.join("New Album");
        assert!(wait_for(&|m| matches!(m, Msg::Changed { path, appeared: true, .. } if *path == album)));
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(album.join("2.flac"), b"y").unwrap();
        let t2 = album.join("2.flac");
        assert!(wait_for(&|m| matches!(m, Msg::Changed { path, .. } if *path == t2)));

        // Deleting the music folder itself is noticed. (Linux only: on
        // Windows the delete waits for the watch's handle to close.)
        #[cfg(target_os = "linux")]
        {
            std::fs::remove_dir_all(&root).unwrap();
            assert!(wait_for(&|m| matches!(m, Msg::State { state: FolderState::Unreachable, .. })));
        }
        // Asked to stop, it does, promptly.
        let asked = Instant::now();
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(asked.elapsed() < Duration::from_secs(3), "took {:?} to stop", asked.elapsed());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn batch_paths_are_always_writable_lines() {
        let ok = PathBuf::from("/m/Artist/Album/1.flac");
        assert_eq!(listable(vec![ok.clone(), ok.clone()]), vec![ok.clone()]);
        let odd = PathBuf::from("/m/Artist/Line\nbreak/1.flac");
        assert_eq!(listable(vec![odd]), vec![PathBuf::from("/m/Artist")]);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = PathBuf::from(std::ffi::OsStr::from_bytes(b"/m/Artist/\xff\xfe.flac"));
            assert_eq!(listable(vec![raw]), vec![PathBuf::from("/m/Artist")]);
        }
    }
}
