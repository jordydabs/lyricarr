// Lyricarr - a lyrics companion for a Lidarr music library.
//
// One executable for Windows or Linux: the window, the command line, the
// folder monitor, the Lidarr hook and the scheduled-scan installer. It
// replaces an earlier Python layout and
// deliberately keeps that version's `config.toml`, `data\lyricarr.db` and
// `.lrc` output, so an existing install's finished work carries straight
// over rather than being re-fetched from LRCLIB.
//
// Double-clicked with no arguments it opens the window; with a subcommand it
// behaves as the command-line tool and the Lidarr hook.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// The folder monitor is ReadDirectoryChangesW on Windows and inotify on
// Linux. Anything else would build a monitor that never hears a thing, so say
// so up front instead.
#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("Lyricarr runs on Windows and Linux.");

mod config;
mod config_edit;
mod gui;
mod logging;
mod lrclib;
mod matcher;
mod os;
mod prefs;
mod scan;
mod store;
mod tags;
mod text;
mod tools;
mod tray;
mod watch;
#[cfg(windows)]
mod winui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "lyricarr",
    version,
    about = "Fetch synced lyrics from LRCLIB into .lrc sidecars beside your music"
)]
struct Cli {
    /// Path to config.toml (default: beside the executable; on Linux,
    /// ~/.config/lyricarr unless there is one beside the executable)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Only errors on stdout
    #[arg(long, global = true)]
    quiet: bool,
    /// Start with no window - in the tray on Windows, as the window-less
    /// monitor on Linux. What the sign-in entry uses, so Lyricarr is there
    /// when you want it without putting a window in front of you at every boot.
    #[arg(long)]
    background: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the window (the default when run with no arguments)
    Gui,
    /// Incremental pass over the library
    Scan {
        /// Stop after N lookups (files already done do not count)
        #[arg(long)]
        limit: Option<u64>,
        /// Look everything up, write nothing
        #[arg(long)]
        dry_run: bool,
        /// Restrict to a folder or a single file (repeat for several)
        #[arg(long)]
        path: Vec<PathBuf>,
        /// Read folders/files to scan from a text file, one per line - what the
        /// folder monitor uses, so a big batch never hits the command-line limit
        #[arg(long, value_name = "FILE")]
        paths_from: Option<PathBuf>,
        /// Re-check every file, ignoring saved state
        #[arg(long)]
        force: bool,
        /// Ignore retry windows and re-ask about past misses
        #[arg(long)]
        retry_now: bool,
        /// Leave files that already carry lyrics in their tags
        #[arg(long)]
        skip_embedded: bool,
    },
    /// Coverage report from the state database
    Stats,
    /// Add a music folder to config.toml, creating config.toml if there is
    /// none yet - Settings -> Add folder, for a machine with no desktop
    AddFolder { dir: PathBuf },
    /// Clear retry windows so the next scan tries them again
    Retry {
        #[arg(long, default_value = "notfound")]
        status: String,
    },
    /// Diagnose one file: tags, variants tried, what matched
    Check { file: PathBuf },
    /// Called by Lidarr's Custom Script connector
    Hook,
    /// Watch the music folders and look up new music as it arrives, with no
    /// window, until stopped. Uses the window's Automation settings.
    Watch,
    /// Register the weekly scheduled scan (a Windows scheduled task, or a
    /// systemd user timer on Linux)
    InstallTask {
        /// Daily or Weekly
        #[arg(long, default_value = "Weekly")]
        schedule: String,
        /// HH:MM
        #[arg(long, default_value = "04:00")]
        time: String,
        /// Remove the task instead
        #[arg(long)]
        remove: bool,
    },
    /// Run `lyricarr watch` as a systemd user service, so new music is
    /// watched for with no window and no desktop session
    #[cfg(not(windows))]
    InstallService {
        /// Remove the service instead
        #[arg(long)]
        remove: bool,
    },
    /// Ask a running scan to stop cleanly
    Stop,
}

fn main() {
    let cli = Cli::parse();
    if let Some(p) = &cli.config {
        config::set_explicit_path(p.clone());
    }
    let cfg = match Config::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e:#}");
            std::process::exit(2);
        }
    };

    let code = match cli.cmd {
        None | Some(Cmd::Gui) => {
            if let Err(e) = gui::run(cfg, cli.background) {
                eprintln!("gui error: {e:#}");
                return;
            }
            0
        }
        Some(cmd) => match run_cli(cmd, &cfg, cli.quiet) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        },
    };
    std::process::exit(code);
}

fn run_cli(cmd: Cmd, cfg: &Config, quiet: bool) -> Result<i32> {
    match cmd {
        Cmd::Gui => unreachable!(),

        Cmd::Scan { limit, dry_run, path, paths_from, force, retry_now, skip_embedded } => {
            let log = logging::Log::open(std::path::Path::new(&cfg.run.log_file), quiet);
            let mut paths = path;
            if let Some(list) = paths_from {
                let text = std::fs::read_to_string(&list)?;
                paths.extend(
                    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from),
                );
                // A list that turns out empty must not quietly become a full
                // scan of every root.
                if paths.is_empty() {
                    log.info(&format!("{} names nothing to scan", list.display()));
                    return Ok(0);
                }
            }
            let args = scan::ScanArgs { limit, dry_run, paths, force, retry_now, skip_embedded };
            scan::run_scan(cfg, &args, &log)
        }

        Cmd::Stats => {
            let s = store::Store::open(std::path::Path::new(&cfg.run.database))?;
            let counts = s.counts()?;
            let total: i64 = counts.values().sum();
            println!("Lyricarr coverage");
            println!("  tracks recorded : {}", total);
            if total == 0 {
                println!("  (nothing scanned yet)");
                return Ok(0);
            }
            for st in store::STATUSES {
                let n = counts.get(*st).copied().unwrap_or(0);
                println!("  {:<14} {:>8}  {:>5.1}%", st, n, 100.0 * n as f64 / total as f64);
            }
            let done: i64 = [store::ST_SYNCED, store::ST_PLAIN, store::ST_INSTRUMENTAL]
                .iter()
                .map(|k| counts.get(*k).copied().unwrap_or(0))
                .sum();
            println!("  ---");
            println!("  with lyrics or known instrumental: {:.1}%", 100.0 * done as f64 / total as f64);
            if let Some(v) = s.get_meta("last_scan") {
                println!("  last scan : {}", v);
            }
            Ok(0)
        }

        Cmd::AddFolder { dir } => {
            let dir = if dir.is_absolute() { dir } else { std::env::current_dir()?.join(dir) };
            if !dir.is_dir() {
                anyhow::bail!("{} is not a folder", dir.display());
            }
            let path = Config::default_config_path();
            Config::ensure_file(&path)?;
            let current = Config::load(Some(&path))?;
            let s = dir.to_string_lossy().to_string();
            if current.library.roots.iter().any(|r| os::same_path(r, &s)) {
                println!("{} is already a music folder in {}", dir.display(), path.display());
                return Ok(0);
            }
            let mut roots = current.library.roots.clone();
            roots.push(s);
            let value = config_edit::toml_path_array(&roots);
            if !config_edit::set_value(&path, "library", "roots", &value)? {
                anyhow::bail!("{} has no roots = ... line under [library]", path.display());
            }
            // Re-read, so a line that somehow did not parse is reported now
            // rather than by the next scan.
            Config::load(Some(&path))?;
            println!("added {} to {}", dir.display(), path.display());
            Ok(0)
        }

        Cmd::Retry { status } => {
            let s = store::Store::open(std::path::Path::new(&cfg.run.database))?;
            let n = s.reset_status(&status)?;
            println!("cleared the retry window on {} '{}' rows", n, status);
            Ok(0)
        }

        Cmd::Check { file } => {
            let md = std::fs::metadata(&file)?;
            let mtime = md.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs_f64();
            let tr = tags::read_track(&file, mtime, md.len() as i64);
            println!("file     : {}", file.display());
            println!("title    : {:?}", tr.title);
            println!("artist   : {:?}", tr.artist);
            println!("albumart : {:?}", tr.albumartist);
            println!("album    : {:?}", tr.album);
            println!("duration : {:.1}s", tr.duration);
            println!("usable   : {}", tr.usable());
            if !tr.usable() {
                println!("\nno usable title/artist tag - this is recorded as 'notags'");
                return Ok(0);
            }
            let stop = Arc::new(AtomicBool::new(false));
            let client = lrclib::Lrclib::new(&cfg.lrclib, stop);
            let m = matcher::Matcher::new(cfg, &client);
            println!("\nvariants tried (most precise first):");
            for (i, (a, t, al)) in m.variants(&tr).iter().enumerate() {
                println!("  {}. artist={:?} title={:?} album={:?}", i + 1, a, t, al);
            }
            let res = m.find(&tr);
            println!("\nresult   : {}", res.status);
            println!("via      : {}", res.note);
            println!("score    : {:.3}", res.score);
            println!("requests : {}", res.requests);
            if let Some(c) = &res.candidate {
                println!(
                    "matched  : #{} {:?} by {:?} ({:.0}s){}",
                    c.id, c.track_name, c.artist_name, c.duration,
                    if c.instrumental { "  [instrumental]" } else { "" }
                );
            }
            if !res.text.is_empty() {
                let first: Vec<&str> = res.text.lines().take(4).collect();
                println!("\nfirst lines ({}):\n  {}", res.kind, first.join("\n  "));
                if let Some(dest) = scan::sidecar_for(&file, &res.kind, cfg) {
                    println!("\nwould write: {}", dest.display());
                }
            }
            Ok(0)
        }

        Cmd::Hook => Ok(hook(cfg)),

        Cmd::Watch => Ok(watch_service(cfg.clone(), quiet)),

        Cmd::InstallTask { schedule, time, remove } => os::install_task(&schedule, &time, remove),

        #[cfg(not(windows))]
        Cmd::InstallService { remove } => os::install_service(remove),

        Cmd::Stop => {
            let p = scan::stop_flag_path(cfg);
            std::fs::write(&p, "stop")?;
            println!("asked the running scan to stop ({})", p.display());
            Ok(0)
        }
    }
}

/// Lidarr Custom Script hook.
///
/// **Always exits 0.** A lyrics problem must never mark a Lidarr import as
/// failed. Lidarr passes the affected paths in the environment, so there is
/// nothing to wire up beyond the path to this executable.
fn hook(cfg: &Config) -> i32 {
    let log = logging::Log::open(std::path::Path::new(&cfg.run.log_file), true);
    let event = std::env::var("lidarr_eventtype").unwrap_or_default();
    if event.eq_ignore_ascii_case("Test") {
        log.info("hook: test event, nothing to do");
        println!("hook: test event, nothing to do");
        return 0;
    }

    let mut paths: Vec<PathBuf> = Vec::new();
    for key in ["lidarr_addedtrackpaths", "lidarr_trackfile_paths", "lidarr_trackfile_path"] {
        if let Ok(v) = std::env::var(key) {
            for part in v.split('|') {
                let p = part.trim();
                if !p.is_empty() {
                    paths.push(PathBuf::from(p));
                }
            }
        }
    }
    // Fall back to the whole artist folder when no individual file was named.
    let mut targets: Vec<PathBuf> = Vec::new();
    if paths.is_empty() {
        if let Ok(v) = std::env::var("lidarr_artist_path") {
            if !v.trim().is_empty() {
                targets.push(PathBuf::from(v.trim()));
            }
        }
    }
    for p in &paths {
        if let Some(d) = p.parent() {
            let d = d.to_path_buf();
            if !targets.contains(&d) {
                targets.push(d);
            }
        }
    }

    if targets.is_empty() {
        log.info(&format!("hook: event {:?} carried no paths, nothing to do", event));
        return 0;
    }

    for t in targets {
        log.info(&format!("hook: scanning {}", t.display()));
        if let Err(e) = scan::run_scan(cfg, &scan::ScanArgs::for_paths(vec![t]), &log) {
            log.info(&format!("hook: scan failed: {e:#}"));
        }
    }
    0
}

/// The folder monitor and the automatic full scan with no window: `lyricarr
/// watch`, and `--background` on Linux.
///
/// Its settings are the window's - prefs.json and config.toml - and it picks
/// up changes to either within a few seconds, so switching monitoring off in
/// the window, or adding a music folder, applies to a monitor running in the
/// background too. If a window is open and already watching, this waits its
/// turn (see watch::Monitor) and takes over when the window closes.
pub fn watch_service(cfg: Config, quiet: bool) -> i32 {
    let log = logging::Log::open(std::path::Path::new(&cfg.run.log_file), quiet);
    let data_dir = std::path::Path::new(&cfg.run.database)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(Config::here);
    let cfg_path = Config::default_config_path();
    let mut prefs = prefs::Prefs::load(&data_dir);
    let mut cfg = cfg;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        // SIGTERM too on Linux (the `termination` feature), which is how
        // systemd stops a service.
        let _ = ctrlc::set_handler(move || s.store(true, std::sync::atomic::Ordering::SeqCst));
    }

    log.info("background monitor started");
    if prefs.scan_on_start && !cfg.library.roots.is_empty() {
        let _ = os::lyricarr().args(["scan", "--quiet"]).spawn();
    }
    let monitor = watch::Monitor::start(prefs.monitor(&cfg), log.clone());

    let stamp = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let prefs_path = prefs::Prefs::path(&data_dir);
    let (mut seen_prefs, mut seen_cfg) = (stamp(&prefs_path), stamp(&cfg_path));
    let mut last_look = std::time::Instant::now();
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if last_look.elapsed() < std::time::Duration::from_secs(3) {
            continue;
        }
        last_look = std::time::Instant::now();
        let (p, c) = (stamp(&prefs_path), stamp(&cfg_path));
        if p == seen_prefs && c == seen_cfg {
            continue;
        }
        // A file caught half-written (or one with a typo in it) keeps the
        // settings already in force rather than dropping to the defaults.
        if p != seen_prefs {
            if let Some(np) = prefs::Prefs::try_load(&data_dir) {
                prefs = np;
                seen_prefs = p;
            }
        }
        if c != seen_cfg {
            match Config::load(Some(&cfg_path)) {
                Ok(nc) => {
                    cfg = nc;
                    seen_cfg = c;
                }
                Err(e) => log.info(&format!("config.toml did not load, keeping the old one: {e:#}")),
            }
        }
        monitor.configure(prefs.monitor(&cfg));
    }
    log.info("background monitor stopping");
    monitor.shutdown();
    0
}
