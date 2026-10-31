//! `config.toml`, with the same keys and the same defaults as the Python.
//!
//! Deliberately the SAME file: the install has tuned values in it (and
//! comments explaining them), so the rewrite reads what is already there
//! rather than inventing a new format and stranding that tuning.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// No default music folder. Guessing where somebody keeps their music is
// worse than having them say so: a wrong guess silently scans nothing (or
// the wrong thing) and looks like a broken program. Settings -> Music
// folders, or `roots` in config.toml.
fn d_roots() -> Vec<String> { Vec::new() }
fn d_exts() -> Vec<String> {
    [".mp3", ".flac", ".m4a", ".ogg", ".opus", ".wma", ".aiff", ".aif", ".wav"]
        .iter().map(|s| s.to_string()).collect()
}
/// `<name>` in the data folder - `data\` beside the executable on Windows.
fn in_data_dir(name: &str) -> String {
    data_dir().join(name).to_string_lossy().into_owned()
}

/// Lyricarr lives beside its executable: config.toml there, state in `data`.
///
/// Always so on Windows, where Lyricarr is a folder you can move or copy and
/// it still knows where its own state is. On Linux only when that is already
/// how it is set up - a config.toml or a data folder beside the binary - since
/// a binary installed to /usr/local/bin cannot write next to itself. Otherwise
/// Linux gets the usual places: ~/.config/lyricarr and ~/.local/share/lyricarr.
fn portable() -> bool {
    if cfg!(windows) {
        return true;
    }
    let d = exe_dir();
    d.join("config.toml").is_file() || d.join("data").is_dir()
}

/// `$XDG_x_HOME/lyricarr`, or `~/<fallback>/lyricarr` when that is unset.
fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
        .map(|p| p.join("lyricarr"))
}

fn config_dir() -> PathBuf {
    if portable() {
        exe_dir()
    } else {
        xdg("XDG_CONFIG_HOME", ".config").unwrap_or_else(exe_dir)
    }
}

fn data_dir() -> PathBuf {
    if portable() {
        exe_dir().join("data")
    } else {
        xdg("XDG_DATA_HOME", ".local/share").unwrap_or_else(|| exe_dir().join("data"))
    }
}

/// A `--config` given on the command line. Set once at startup.
static EXPLICIT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Remember `--config`, so every scan this process starts uses the same file.
pub fn set_explicit_path(p: PathBuf) {
    let _ = EXPLICIT.set(p);
}

/// The `--config` this process was started with, if any.
pub fn explicit_path() -> Option<&'static Path> {
    EXPLICIT.get().map(|p| p.as_path())
}

/// The commented example, written out as config.toml when there is none yet.
pub const EXAMPLE: &str = include_str!("../config.example.toml");

/// The folder the executable lives in, so the program works the same whether
/// it is launched by a double-click, a scheduled task with no working
/// directory, or Lidarr.
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn d_skip() -> Vec<String> {
    [".recycle", "$recycle.bin", "system volume information", ".stversions"]
        .iter().map(|s| s.to_string()).collect()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Library {
    pub roots: Vec<String>,
    pub audio_extensions: Vec<String>,
    pub skip_dir_names: Vec<String>,
}
impl Default for Library {
    fn default() -> Self {
        Self { roots: d_roots(), audio_extensions: d_exts(), skip_dir_names: d_skip() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Output {
    /// "lrc" | "txt" | "skip" - what to do when only unsynced lyrics exist.
    pub plain_lyrics: String,
    pub overwrite: bool,
    pub add_id_tags: bool,
    pub instrumental_marker: bool,
    pub min_lyrics_chars: usize,
}
impl Default for Output {
    fn default() -> Self {
        Self {
            plain_lyrics: "lrc".into(),
            overwrite: false,
            add_id_tags: false,
            instrumental_marker: false,
            min_lyrics_chars: 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Lrclib {
    pub host: String,
    pub requests_per_second: f64,
    pub workers: usize,
    pub timeout_seconds: f64,
    pub max_retries: u32,
    pub duration_tolerance_get: f64,
    pub duration_tolerance_search: f64,
    pub min_score: f64,
    pub min_title_ratio: f64,
    pub plain_duration_tolerance: f64,
    pub plain_min_title_ratio: f64,
    pub plain_min_artist_ratio: f64,
}
impl Default for Lrclib {
    fn default() -> Self {
        Self {
            host: "lrclib.net".into(),
            requests_per_second: 3.0,
            workers: 4,
            timeout_seconds: 20.0,
            max_retries: 4,
            duration_tolerance_get: 5.0,
            duration_tolerance_search: 10.0,
            min_score: 0.80,
            min_title_ratio: 0.72,
            plain_duration_tolerance: 45.0,
            plain_min_title_ratio: 0.90,
            plain_min_artist_ratio: 0.80,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Run {
    pub database: String,
    pub log_file: String,
    pub log_level: String,
    pub progress_seconds: u64,
}
impl Default for Run {
    fn default() -> Self {
        Self {
            // Beside the executable on Windows, so Lyricarr is a folder you
            // can move or copy somewhere else and it still knows where its own
            // state is. See `portable` for Linux.
            database: in_data_dir("lyricarr.db"),
            log_file: in_data_dir("lyricarr.log"),
            log_level: "INFO".into(),
            progress_seconds: 15,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub library: Library,
    pub output: Output,
    pub lrclib: Lrclib,
    pub run: Run,
}

impl Config {
    /// Load `config.toml` from beside the executable, falling back to the
    /// built-in defaults when it is absent. Unknown keys are ignored so an
    /// older config keeps working.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path(),
        };
        if !path.is_file() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// The config.toml in use: `--config` if one was given, otherwise the
    /// one in the usual place.
    pub fn default_config_path() -> PathBuf {
        explicit_path()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| config_dir().join("config.toml"))
    }

    /// Make sure there is a config.toml to edit, writing the commented example
    /// if there is none. The window edits that file line by line, and with no
    /// file there used to say "config.toml does not exist" the first time
    /// someone tried to add a music folder.
    pub fn ensure_file(path: &Path) -> Result<()> {
        use std::io::Write;
        if path.is_file() {
            return Ok(());
        }
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        // create_new, so one that appeared in the meantime is never replaced.
        match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => f
                .write_all(EXAMPLE.as_bytes())
                .with_context(|| format!("writing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }

    /// The folder the executable lives in, so the program works the same
    /// whether it is launched by a double-click, a scheduled task with no
    /// working directory, or Lidarr.
    pub fn here() -> PathBuf {
        exe_dir()
    }

    pub fn is_audio(&self, path: &Path) -> bool {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => {
                let dotted = format!(".{}", ext.to_lowercase());
                self.library.audio_extensions.iter().any(|e| e.eq_ignore_ascii_case(&dotted))
            }
            None => false,
        }
    }

    pub fn skips_dir(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        // Plus the Linux desktop's trash on a removable drive, `.Trash-1000`:
        // the number is the user's id, so it cannot be listed by name.
        lower.starts_with(".trash-")
            || self.library.skip_dir_names.iter().any(|s| s.to_lowercase() == lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_is_a_valid_config_that_matches_the_defaults() {
        let c: Config = toml::from_str(EXAMPLE).expect("config.example.toml should parse");
        let d = Config::default();
        assert!(c.library.roots.is_empty());
        assert_eq!(c.library.audio_extensions, d.library.audio_extensions);
        assert_eq!(c.lrclib.requests_per_second, d.lrclib.requests_per_second);
        assert_eq!(c.output.plain_lyrics, d.output.plain_lyrics);
    }

    #[test]
    fn ensure_file_writes_the_example_once_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("lyr-ensure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("sub").join("config.toml");
        Config::ensure_file(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), EXAMPLE);
        std::fs::write(&p, "roots = ['x']\n").unwrap();
        Config::ensure_file(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "roots = ['x']\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trash_folders_are_skipped() {
        let c = Config::default();
        assert!(c.skips_dir(".Trash-1000"));
        assert!(c.skips_dir("$RECYCLE.BIN"));
        assert!(!c.skips_dir("Trash Talk"));
    }
}
