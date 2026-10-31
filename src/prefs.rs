//! Window preferences - tray behaviour, readability, what the table shows.
//!
//! Kept OUT of `config.toml` on purpose. That file is hand-written and heavily
//! commented, and the comments are worth more than the convenience of letting
//! the window rewrite it wholesale. Engine settings that the Settings tab does
//! edit (`requests_per_second`, `overwrite`, ...) are changed in place with a
//! surgical line edit instead - see `config_edit.rs`.
//!
//! Saved on every change rather than at exit, so nothing is lost when the
//! process is ended from the tray.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Minimising hides to the tray instead of to the taskbar.
    pub minimize_to_tray: bool,
    /// The X button hides to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Launch in the background at sign-in (Windows: the tray; Linux: the
    /// window-less monitor). The name predates Linux, and stays so that an
    /// existing prefs.json keeps its value. See os::set_start_at_login.
    pub start_with_windows: bool,
    /// Kick off a scan as soon as the program starts - the catch-up for
    /// anything added while Lyricarr was not running to see it.
    pub scan_on_start: bool,
    /// Watch the music folders and look up new tracks as they arrive.
    pub watch_folders: bool,
    /// Seconds a new file must be left alone before it is looked up, so a
    /// copy still in progress is never read half-written.
    pub watch_settle_secs: u32,
    /// Run a full scan this often while Lyricarr is running; 0 is never.
    pub auto_scan_hours: u32,
    /// 0.9 = compact, 1.0 = normal, up to 1.4 for easier reading.
    pub ui_scale: f32,
    /// Extra vertical room in the table - the difference between a dense list
    /// and one you can follow across the screen.
    pub row_height: f32,
    /// How many rows the table asks the database for.
    pub row_limit: usize,
    /// Last status filter and search, restored next time.
    pub filter: String,
    pub needle: String,
    /// Which column the table is sorted by, as an index into `SortCol::ALL`.
    pub sort_col: usize,
    pub sort_desc: bool,
    /// Bumped by "Reset column widths": it is part of the table's id, so a new
    /// value gives a table with no remembered widths.
    pub table_gen: u32,
    /// Which tab was open.
    pub tab: usize,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            minimize_to_tray: false,
            close_to_tray: true,
            start_with_windows: false,
            scan_on_start: false,
            // On by default: what nobody has to remember to do is the point.
            // An existing prefs.json gains these on its first load.
            watch_folders: true,
            watch_settle_secs: 10,
            auto_scan_hours: 24,
            ui_scale: 1.0,
            row_height: 24.0,
            row_limit: 500,
            filter: "notfound".into(),
            needle: String::new(),
            sort_col: 1, // artist
            sort_desc: false,
            table_gen: 0,
            tab: 0,
        }
    }
}

impl Prefs {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("prefs.json")
    }

    pub fn load(data_dir: &Path) -> Self {
        Self::try_load(data_dir).unwrap_or_else(|| Self::default().clamped())
    }

    /// The saved preferences, or None when there are none or they do not
    /// parse - for the background service, which must keep the settings it
    /// has rather than fall back to the defaults over one bad read.
    pub fn try_load(data_dir: &Path) -> Option<Self> {
        let p: Self = std::fs::read_to_string(Self::path(data_dir))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())?;
        Some(p.clamped())
    }

    fn clamped(mut self) -> Self {
        let p = &mut self;
        // A saved file from a hand edit should not be able to make the window
        // unusable, so clamp rather than trust.
        p.ui_scale = p.ui_scale.clamp(0.8, 1.6);
        p.row_height = p.row_height.clamp(18.0, 44.0);
        p.row_limit = p.row_limit.clamp(50, 5000);
        // The range the Settings slider offers.
        p.watch_settle_secs = p.watch_settle_secs.clamp(3, 120);
        p.auto_scan_hours = p.auto_scan_hours.min(24 * 30);
        if p.sort_col >= crate::store::SortCol::ALL.len() {
            p.sort_col = 1;
        }
        self
    }

    /// Written to a temporary file and renamed into place, so that nothing -
    /// the background service re-reading it, a crash halfway - ever sees half
    /// a file.
    pub fn save(&self, data_dir: &Path) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::create_dir_all(data_dir);
            let dest = Self::path(data_dir);
            let tmp = dest.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() && std::fs::rename(&tmp, &dest).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// What the folder monitor should be doing, given these preferences.
    pub fn monitor(&self, cfg: &crate::config::Config) -> crate::watch::Settings {
        crate::watch::Settings {
            watch: self.watch_folders,
            settle: std::time::Duration::from_secs(self.watch_settle_secs as u64),
            auto_scan_hours: self.auto_scan_hours,
            cfg: cfg.clone(),
        }
    }

    pub fn sort(&self) -> crate::store::Sort {
        crate::store::Sort {
            col: crate::store::SortCol::ALL[self.sort_col.min(crate::store::SortCol::ALL.len() - 1)],
            desc: self.sort_desc,
        }
    }
}
