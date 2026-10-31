//! The scan: walk, decide, look up, write, record.
//!
//! Resumable and idempotent by design. Stopping it is safe at any moment -
//! tracks already in flight finish, their state is committed, and the next run
//! picks up exactly where this one left off. That matters because a first full
//! pass over a large library is a multi-day job.

use crate::config::Config;
use crate::lrclib::Lrclib;
use crate::matcher::Matcher;
use crate::store::{
    self, Row, Store, ST_ERROR, ST_INSTRUMENTAL, ST_NOTAGS, ST_NOTFOUND, ST_PLAIN, ST_SYNCED,
};
use crate::tags::{has_embedded_lyrics, read_track, Track};
use anyhow::Result;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Totals {
    pub seen: AtomicU64,
    pub queued: AtomicU64,
    pub skipped_done: AtomicU64,
    pub skipped_existing: AtomicU64,
    pub skipped_retry: AtomicU64,
    pub skipped_embedded: AtomicU64,
    pub notags: AtomicU64,
    pub synced: AtomicU64,
    pub plain: AtomicU64,
    pub instrumental: AtomicU64,
    pub notfound: AtomicU64,
    pub error: AtomicU64,
    pub written: AtomicU64,
}

impl Totals {
    fn bump(&self, f: &AtomicU64) {
        f.fetch_add(1, Ordering::Relaxed);
    }
    pub fn json(&self) -> String {
        let g = |f: &AtomicU64| f.load(Ordering::Relaxed);
        format!(
            r#"{{"seen": {}, "queued": {}, "skipped_done": {}, "skipped_existing": {}, "skipped_retry": {}, "skipped_embedded": {}, "notags": {}, "synced": {}, "plain": {}, "instrumental": {}, "notfound": {}, "error": {}, "written": {}}}"#,
            g(&self.seen), g(&self.queued), g(&self.skipped_done), g(&self.skipped_existing),
            g(&self.skipped_retry), g(&self.skipped_embedded), g(&self.notags), g(&self.synced),
            g(&self.plain), g(&self.instrumental), g(&self.notfound), g(&self.error),
            g(&self.written)
        )
    }
}

pub struct ScanArgs {
    pub limit: Option<u64>,
    pub dry_run: bool,
    /// Folders or single files to look at instead of the library roots - the
    /// Lidarr hook, the folder monitor and "scan just this". Empty means a
    /// full scan of every root.
    pub paths: Vec<PathBuf>,
    pub force: bool,
    pub retry_now: bool,
    pub skip_embedded: bool,
}

impl ScanArgs {
    /// A targeted run over a few paths, with everything else at its default.
    pub fn for_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            limit: None, dry_run: false, paths, force: false,
            retry_now: false, skip_embedded: false,
        }
    }

    /// The full pass over every root, as opposed to a targeted one. Only a full
    /// scan takes the scan lock and owns the heartbeat the window reads.
    pub fn is_full(&self) -> bool {
        self.paths.is_empty()
    }
}

// ---------------------------------------------------------------------------
// sidecars
// ---------------------------------------------------------------------------

/// Where a sidecar for this kind of payload goes, or `None` when the config
/// says not to write unsynced lyrics at all.
pub fn sidecar_for(path: &Path, kind: &str, cfg: &Config) -> Option<PathBuf> {
    let stem = path.with_extension("");
    if kind == "synced" {
        return Some(stem.with_extension("lrc"));
    }
    match cfg.output.plain_lyrics.as_str() {
        "skip" => None,
        "txt" => Some(stem.with_extension("txt")),
        _ => Some(stem.with_extension("lrc")),
    }
}

/// Non-empty sidecars already sitting beside the audio file.
pub fn existing_sidecars(path: &Path) -> Vec<PathBuf> {
    let stem = path.with_extension("");
    ["lrc", "txt"]
        .iter()
        .map(|e| stem.with_extension(e))
        .filter(|p| p.metadata().map(|m| m.is_file() && m.len() > 0).unwrap_or(false))
        .collect()
}

fn lrc_header(tr: &Track) -> String {
    let mut bits: Vec<String> = Vec::new();
    if !tr.title.is_empty() {
        bits.push(format!("[ti:{}]", tr.title));
    }
    if !tr.search_artist().is_empty() {
        bits.push(format!("[ar:{}]", tr.search_artist()));
    }
    if !tr.album.is_empty() {
        bits.push(format!("[al:{}]", tr.album));
    }
    if tr.duration > 0.0 {
        let total = tr.duration.round() as i64;
        bits.push(format!("[length:{:02}:{:02}]", total / 60, total % 60));
    }
    if bits.is_empty() {
        String::new()
    } else {
        format!("{}\n", bits.join("\n"))
    }
}

/// Atomic write, so a crash or a yanked drive cannot leave a half file beside
/// somebody's music.
fn write_text(dest: &Path, text: &str) -> std::io::Result<()> {
    let tmp = dest.with_extension(format!(
        "{}.lyricarr.tmp",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("lrc")
    ));
    let body = {
        let mut s = text.replace("\r\n", "\n");
        while s.ends_with('\n') {
            s.pop();
        }
        s.push('\n');
        s
    };
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, dest)
}

// ---------------------------------------------------------------------------
// work decision
// ---------------------------------------------------------------------------

fn needs_work(
    path: &Path,
    row: Option<&Row>,
    mtime: f64,
    size: i64,
    cfg: &Config,
    args: &ScanArgs,
    t: &Totals,
) -> bool {
    if args.force {
        return true;
    }
    let have = existing_sidecars(path);
    if !have.is_empty() && !cfg.output.overwrite {
        if row.is_none() {
            t.bump(&t.skipped_existing);
        } else {
            t.bump(&t.skipped_done);
        }
        return false;
    }
    let Some(row) = row else { return true };

    let changed = (row.mtime - mtime).abs() > 2.0 || row.size != size;
    if changed {
        return true;
    }
    match row.status.as_str() {
        ST_SYNCED | ST_PLAIN => {
            // Recorded as done but the sidecar is gone -> redo it.
            if !row.lrc_path.is_empty() && !Path::new(&row.lrc_path).is_file() {
                return true;
            }
            t.bump(&t.skipped_done);
            false
        }
        ST_INSTRUMENTAL => {
            t.bump(&t.skipped_done);
            false
        }
        ST_NOTAGS => {
            t.bump(&t.notags);
            false
        }
        ST_NOTFOUND | ST_ERROR => {
            if args.retry_now || row.next_retry.trim().is_empty() {
                return true;
            }
            match store::parse_iso(&row.next_retry) {
                Some(due) if due > chrono::Utc::now() => {
                    t.bump(&t.skipped_retry);
                    false
                }
                _ => true,
            }
        }
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

/// A file the GUI can drop to ask a running scan to stop. Polled rather than
/// signalled: the Python used a PID file plus CTRL_BREAK, which needed console
/// group plumbing to get right; a flag file does the same job and the scanner
/// still finishes what it has in flight.
///
/// A scan stops when the flag is *touched after it started*, rather than when
/// the flag merely exists. Each scan used to delete the flag on sight, so with
/// a hook or monitor run going alongside the full scan, whichever polled first
/// swallowed the request and the full scan carried on regardless. Comparing
/// against the flag's own modification time (not the clock) also keeps a data
/// folder on a share with a skewed clock from confusing it.
pub fn stop_flag_path(cfg: &Config) -> PathBuf {
    Path::new(&cfg.run.database)
        .parent()
        .unwrap_or(&Config::here())
        .join("stop.flag")
}

fn flag_stamp(flag: &Path) -> Option<std::time::SystemTime> {
    flag.metadata().and_then(|m| m.modified()).ok()
}

fn pid_file(cfg: &Config) -> PathBuf {
    data_dir(cfg).join("scan.pid")
}

/// The file whose exclusive open IS the "a full scan is running" lock. Windows
/// releases it even if the process is killed, which a pid file cannot promise.
fn scan_lock_path(cfg: &Config) -> PathBuf {
    data_dir(cfg).join("scan.lock")
}

fn data_dir(cfg: &Config) -> PathBuf {
    Path::new(&cfg.run.database)
        .parent()
        .unwrap_or(&Config::here())
        .to_path_buf()
}

/// "D:\Music\A\B" or "D:\Music\A\B (+3 more)", for log lines about a
/// targeted run.
fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [] => String::new(),
        [one] => one.display().to_string(),
        [first, rest @ ..] => format!("{} (+{} more)", first.display(), rest.len()),
    }
}

/// How one lookup came out, in words, for the per-track lines a targeted run
/// writes to the log.
fn outcome(status: &str) -> &'static str {
    match status {
        ST_SYNCED => "synced lyrics",
        ST_PLAIN => "plain lyrics",
        ST_INSTRUMENTAL => "instrumental",
        ST_NOTFOUND => "no lyrics on LRCLIB yet",
        _ => "could not reach LRCLIB, will retry",
    }
}

pub fn run_scan(cfg: &Config, args: &ScanArgs, log: &crate::logging::Log) -> Result<i32> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        // Ctrl-C finishes in-flight work rather than killing it.
        let _ = ctrlc::set_handler(move || s.store(true, Ordering::SeqCst));
    }

    let full = args.is_full();
    let roots: Vec<PathBuf> = if full {
        cfg.library.roots.iter().map(PathBuf::from).collect()
    } else {
        args.paths.clone()
    };
    if roots.is_empty() {
        // Otherwise this finishes in 0:00:00 having done nothing, which reads
        // as success. Say what is missing and how to fix it. Checked before the
        // heartbeat is claimed: claiming it first left the window saying
        // SCANNING for a minute after a scan that never started.
        log.info(
            "no music folders configured - nothing to scan. \
             Add one in Settings -> Music folders, or set `roots` under \
             [library] in config.toml.",
        );
        return Ok(2);
    }

    let totals = Arc::new(Totals::default());
    let store = Store::open(Path::new(&cfg.run.database))?;
    let store = Arc::new(Mutex::new(store));

    // One full scan at a time.
    //
    // scan.pid used to be written here and never read by anything, so nothing
    // stopped a second pass piling on top of a running one - and "start a scan
    // when Lyricarr opens", the tray's Start scan and the weekly task can each
    // fire without knowing about the others. Two full passes would double the
    // request rate against a free, donation-funded service and redo each
    // other's work.
    //
    // A targeted run is deliberately exempt: that is the Lidarr hook or the
    // folder monitor reacting to new music, it is a handful of files, and
    // refusing it because the weekly scan happens to be running would mean
    // new music silently waits a week for its lyrics.
    let scan_lock = if full {
        match crate::os::exclusive_lock(&scan_lock_path(cfg)) {
            Some(f) => Some(f),
            None => {
                log.info("a scan is already running - leaving it to finish (lyricarr stop ends it)");
                return Ok(0);
            }
        }
    } else {
        None
    };

    // Claim the heartbeat straight away. It is otherwise only written on the
    // progress tick, which leaves a window of several seconds at startup in
    // which the GUI still says Idle and a second scan would see no one home.
    //
    // A targeted run has a heartbeat of its own. It used to share the full
    // scan's, so a hook run going alongside the weekly pass overwrote that
    // pass's progress line with its own, and the window flicked between the
    // two sets of numbers.
    {
        let s = store.lock().unwrap();
        let now = store::now_iso();
        if full {
            let _ = std::fs::write(pid_file(cfg), std::process::id().to_string());
            let _ = s.set_meta("scan_heartbeat", &now);
            // Stamped at the start as well as at the end, so an automatic full
            // scan that keeps dying part-way is retried on its schedule rather
            // than restarted every minute.
            let _ = s.set_meta("last_full_scan_started", &now);
        } else {
            let _ = s.set_meta("quick_heartbeat", &now);
        }
    }
    let flag = stop_flag_path(cfg);
    let flag_at_start = flag_stamp(&flag);

    // A folder that is not reachable (an unmapped drive) looks identical to an
    // empty one from in here, so it is worth saying out loud.
    for r in &roots {
        if !r.exists() {
            if full {
                log.info(&format!("music folder not reachable, skipping: {}", r.display()));
            } else {
                log.info(&format!("gone before it could be checked, skipping: {}", r.display()));
            }
        }
    }

    let flags = format!(
        "{}{}{}",
        if args.dry_run { " [dry-run]" } else { "" },
        if args.force { " [force]" } else { "" },
        args.limit.map(|n| format!(" [limit {}]", n)).unwrap_or_default(),
    );
    if full {
        log.info(&format!(
            "scan start: {} root(s){}{}",
            roots.len(),
            if args.retry_now { " [retry-now]" } else { "" },
            flags,
        ));
    } else {
        log.info(&format!("checking {}{}", describe(&roots), flags));
    }

    // ---- producer: walk and filter ---------------------------------------
    // jwalk reads directories on a small thread pool. On a library of a few
    // hundred thousand files - especially over a network share - that is the
    // difference between a walk you wait through and one you do not notice.
    let queue: Arc<Mutex<VecDeque<(PathBuf, f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    let producing = Arc::new(AtomicBool::new(true));
    let lookups = Arc::new(AtomicU64::new(0));

    let prod = {
        let (cfg, totals, store, queue, stop, producing) = (
            cfg.clone(), totals.clone(), store.clone(), queue.clone(), stop.clone(),
            producing.clone(),
        );
        let pargs = ScanArgs {
            limit: None, dry_run: false, paths: Vec::new(),
            force: args.force, retry_now: args.retry_now,
            skip_embedded: args.skip_embedded,
        };
        let roots = roots.clone();
        std::thread::spawn(move || {
            // A targeted run can be handed a folder and files inside it - the
            // monitor and the hook both can - and without this a track would
            // be looked up twice, by two workers at once. A full scan has no
            // such overlap and is too large to be worth remembering.
            let mut queued: Option<std::collections::HashSet<PathBuf>> =
                if full { None } else { Some(Default::default()) };

            // skip_dir_names, checked per component so a skip folder anywhere
            // up the tree excludes what is under it
            let wanted = |p: &Path| {
                !p.components().any(|c| cfg.skips_dir(&c.as_os_str().to_string_lossy()))
                    && cfg.is_audio(p)
            };

            let mut consider = |p: PathBuf, md: Option<std::fs::Metadata>| {
                if let Some(set) = queued.as_mut() {
                    if !set.insert(p.clone()) {
                        return;
                    }
                }
                totals.seen.fetch_add(1, Ordering::Relaxed);
                let Some(md) = md else { return };
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let size = md.len() as i64;

                let row = { store.lock().unwrap().get(&p.to_string_lossy()).ok().flatten() };
                if !needs_work(&p, row.as_ref(), mtime, size, &cfg, &pargs, &totals) {
                    return;
                }
                if pargs.skip_embedded && has_embedded_lyrics(&p) {
                    totals.skipped_embedded.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                totals.queued.fetch_add(1, Ordering::Relaxed);
                queue.lock().unwrap().push_back((p, mtime, size));

                // Don't let the walk run arbitrarily far ahead of the
                // workers; LRCLIB is the bottleneck, not the disk.
                while queue.lock().unwrap().len() > 512 && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            };

            for root in roots {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // A single track: the monitor and the hook name exact files.
                if root.is_file() {
                    if wanted(&root) {
                        consider(root.clone(), std::fs::metadata(&root).ok());
                    }
                    continue;
                }
                for entry in jwalk::WalkDir::new(&root).skip_hidden(false) {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(e) = entry else { continue };
                    if e.file_type().is_dir() {
                        continue;
                    }
                    let p = e.path();
                    if !wanted(&p) {
                        continue;
                    }
                    let md = e.metadata().ok();
                    consider(p, md);
                }
            }
            producing.store(false, Ordering::SeqCst);
        })
    };

    // ---- consumers --------------------------------------------------------
    let client = Arc::new(Lrclib::new(&cfg.lrclib, stop.clone()));
    let n_workers = cfg.lrclib.workers.max(1);
    let mut workers = Vec::new();
    for _ in 0..n_workers {
        let (cfg, totals, store, queue, stop, client, producing, lookups) = (
            cfg.clone(), totals.clone(), store.clone(), queue.clone(), stop.clone(),
            client.clone(), producing.clone(), lookups.clone(),
        );
        let dry_run = args.dry_run;
        let limit = args.limit;
        let log = log.clone();
        workers.push(std::thread::spawn(move || {
            let matcher = Matcher::new(&cfg, &client);
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(n) = limit {
                    if lookups.load(Ordering::Relaxed) >= n {
                        break;
                    }
                }
                let item = queue.lock().unwrap().pop_front();
                let Some((path, mtime, size)) = item else {
                    if !producing.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(40));
                    continue;
                };

                let tr = read_track(&path, mtime, size);
                let now = store::now_iso();

                if !tr.usable() {
                    totals.notags.fetch_add(1, Ordering::Relaxed);
                    if !full {
                        log.info(&format!("no usable title/artist tag: {}", path.display()));
                    }
                    let row = Row {
                        path: tr.path.clone(), mtime, size,
                        artist: tr.artist.clone(), album: tr.album.clone(),
                        title: tr.title.clone(), duration: tr.duration,
                        status: ST_NOTAGS.into(), kind: String::new(),
                        lrc_path: String::new(), lrclib_id: None, score: 0.0,
                        attempts: 0, first_seen: now.clone(), last_attempt: now.clone(),
                        next_retry: String::new(),
                        note: "no usable title/artist tag".into(),
                    };
                    let _ = store.lock().unwrap().record(&row);
                    continue;
                }

                lookups.fetch_add(1, Ordering::Relaxed);
                let m = matcher.find(&tr);

                // write the sidecar
                let mut lrc_path = String::new();
                if !m.text.is_empty() && !dry_run {
                    if let Some(dest) = sidecar_for(&path, &m.kind, &cfg) {
                        let already = dest.metadata().map(|x| x.is_file() && x.len() > 0).unwrap_or(false);
                        if already && !cfg.output.overwrite {
                            lrc_path = dest.to_string_lossy().to_string();
                        } else {
                            let body = if m.kind == "synced" && cfg.output.add_id_tags {
                                format!("{}{}", lrc_header(&tr), m.text)
                            } else {
                                m.text.clone()
                            };
                            match write_text(&dest, &body) {
                                Ok(()) => {
                                    totals.written.fetch_add(1, Ordering::Relaxed);
                                    lrc_path = dest.to_string_lossy().to_string();
                                }
                                Err(e) => log.info(&format!(
                                    "write failed {}: {}", dest.display(), e
                                )),
                            }
                        }
                    }
                } else if m.status == ST_INSTRUMENTAL && cfg.output.instrumental_marker && !dry_run {
                    if let Some(dest) = sidecar_for(&path, "synced", &cfg) {
                        let _ = write_text(&dest, "[by:lyricarr] instrumental");
                        lrc_path = dest.to_string_lossy().to_string();
                    }
                }

                match m.status {
                    ST_SYNCED => totals.synced.fetch_add(1, Ordering::Relaxed),
                    ST_PLAIN => totals.plain.fetch_add(1, Ordering::Relaxed),
                    ST_INSTRUMENTAL => totals.instrumental.fetch_add(1, Ordering::Relaxed),
                    ST_NOTFOUND => totals.notfound.fetch_add(1, Ordering::Relaxed),
                    _ => totals.error.fetch_add(1, Ordering::Relaxed),
                };
                // One line per track on a targeted run, so the Activity tab
                // shows what happened to the music that just arrived. A full
                // scan would bury the log in them.
                if !full {
                    log.info(&format!(
                        "looked up {} - {}: {}",
                        tr.search_artist(),
                        tr.title,
                        outcome(m.status)
                    ));
                }

                let prev_attempts = {
                    let s = store.lock().unwrap();
                    s.get(&tr.path).ok().flatten().map(|r| r.attempts).unwrap_or(0)
                };
                let attempts = prev_attempts + 1;
                let row = Row {
                    path: tr.path.clone(), mtime, size,
                    artist: tr.search_artist().to_string(),
                    album: tr.album.clone(), title: tr.title.clone(),
                    duration: tr.duration,
                    status: m.status.into(),
                    kind: m.kind.clone(),
                    lrc_path,
                    lrclib_id: m.candidate.as_ref().map(|c| c.id).filter(|i| *i != 0),
                    score: m.score,
                    attempts,
                    first_seen: now.clone(),
                    last_attempt: now.clone(),
                    next_retry: Store::next_retry_for(m.status, attempts).unwrap_or_default(),
                    note: m.note.clone(),
                };
                let _ = store.lock().unwrap().record(&row);
            }
        }));
    }

    // ---- progress ---------------------------------------------------------
    let started = Instant::now();
    let every = Duration::from_secs(cfg.run.progress_seconds.max(1));
    let mut last = Instant::now();
    let mut last_log = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        if !stop.load(Ordering::Relaxed) {
            if let Some(now) = flag_stamp(&flag) {
                if Some(now) != flag_at_start {
                    log.info("stop requested - finishing tracks already in flight");
                    stop.store(true, Ordering::SeqCst);
                }
            }
        }
        let done = !producing.load(Ordering::Relaxed)
            && queue.lock().unwrap().is_empty()
            && workers.iter().all(|w| w.is_finished());
        // A targeted run re-stamps its own heartbeat far more often than the
        // full scan does: it is usually over in seconds, and the window counts
        // it as finished once the stamp is 20 seconds old.
        let tick = if full { every } else { Duration::from_secs(5) };
        if last.elapsed() >= tick {
            last = Instant::now();
            // Heartbeat, so the window can say SCANNING without having to ask
            // Windows whether a pid is still alive. Stale by definition once
            // the scan exits, which is exactly the signal wanted.
            //
            // The live figures go with it. Without them the only sign that a
            // scan was running was six headline counts creeping up by one or
            // two a minute, which reads exactly like a program doing nothing.
            if let Ok(s) = store.try_lock() {
                if full {
                    let _ = s.set_meta("scan_heartbeat", &store::now_iso());
                    let g = |f: &AtomicU64| f.load(Ordering::Relaxed);
                    let secs = started.elapsed().as_secs_f64().max(0.001);
                    let n = lookups.load(Ordering::Relaxed);
                    let _ = s.set_meta(
                        "scan_progress",
                        &format!(
                            "{{\"seen\":{},\"lookups\":{},\"synced\":{},\"plain\":{},\"instrumental\":{},\"notfound\":{},\"error\":{},\"rate\":{:.2},\"elapsed\":{:.0}}}",
                            g(&totals.seen), n, g(&totals.synced), g(&totals.plain),
                            g(&totals.instrumental), g(&totals.notfound), g(&totals.error),
                            n as f64 / secs, secs,
                        ),
                    );
                } else {
                    let _ = s.set_meta("quick_heartbeat", &store::now_iso());
                }
            }
        }
        // NOT gated on args.quiet. Log::quiet already means "do not print to
        // stdout" and writes the file regardless; this extra condition meant a
        // scan started from the window or the tray - which always pass
        // --quiet - wrote not one progress line, so the Activity tab stayed
        // empty for six hours and the program looked dead.
        if last_log.elapsed() >= every {
            last_log = Instant::now();
            let g = |f: &AtomicU64| f.load(Ordering::Relaxed);
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let line = format!(
                "seen {} | tried {} | synced {} plain {} instr {} miss {} err {} | {:.1} tracks/s | api {} (retry {})",
                g(&totals.seen), lookups.load(Ordering::Relaxed), g(&totals.synced),
                g(&totals.plain), g(&totals.instrumental), g(&totals.notfound), g(&totals.error),
                lookups.load(Ordering::Relaxed) as f64 / secs,
                client.stats.requests.load(Ordering::Relaxed),
                client.stats.retries.load(Ordering::Relaxed),
            );
            log.info(&line);
        }
        if done || (stop.load(Ordering::Relaxed) && workers.iter().all(|w| w.is_finished())) {
            break;
        }
    }

    stop.store(true, Ordering::SeqCst);
    let _ = prod.join();
    for w in workers {
        let _ = w.join();
    }

    {
        let s = store.lock().unwrap();
        let now = store::now_iso();
        let _ = s.set_meta("last_scan", &now);
        let _ = s.set_meta("last_scan_totals", &totals.json());
        if full {
            // What the monitor's automatic full scan counts from. last_scan
            // will not do: every hook and monitor run moves it, so a library
            // with music arriving daily would never be due a full pass.
            let _ = s.set_meta("last_full_scan", &now);
            // Blank the heartbeat so the window flips to Idle at once.
            let _ = s.set_meta("scan_heartbeat", "");
            let _ = s.set_meta("scan_progress", "");
        } else {
            // Blanked even if another targeted run is still going: that one
            // re-stamps it within five seconds, well inside what the window
            // allows before calling it finished.
            let _ = s.set_meta("quick_heartbeat", "");
        }
    }
    if full {
        let _ = std::fs::remove_file(pid_file(cfg));
    }
    drop(scan_lock);

    let el = started.elapsed();
    let g = |f: &AtomicU64| f.load(Ordering::Relaxed);
    log.info(&format!(
        "finished in {}:{:02}:{:02} - {} looked up: {} synced, {} plain, {} instrumental, {} no match, {} errors",
        el.as_secs() / 3600, (el.as_secs() % 3600) / 60, el.as_secs() % 60,
        lookups.load(Ordering::Relaxed), g(&totals.synced), g(&totals.plain),
        g(&totals.instrumental), g(&totals.notfound), g(&totals.error),
    ));
    Ok(0)
}
