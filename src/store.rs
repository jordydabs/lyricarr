//! The state database.
//!
//! The SAME file and the SAME schema as the Python version, on purpose: an
//! existing install has tens of thousands of rows of finished work in it, and
//! a fresh schema would throw that away and re-ask LRCLIB for all of it. The
//! `CREATE TABLE IF NOT EXISTS` below therefore has to stay compatible with
//! what is already on disk.
//!
//! Timestamps are written in the same format the Python used - RFC3339 with a
//! `+00:00` offset - because `next_retry` rows already in the table are
//! compared against them.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

pub const ST_SYNCED: &str = "synced";
pub const ST_PLAIN: &str = "plain";
pub const ST_INSTRUMENTAL: &str = "instrumental";
pub const ST_NOTFOUND: &str = "notfound";
pub const ST_NOTAGS: &str = "notags";
pub const ST_ERROR: &str = "error";

pub const STATUSES: &[&str] =
    &[ST_SYNCED, ST_PLAIN, ST_INSTRUMENTAL, ST_NOTFOUND, ST_ERROR, ST_NOTAGS];

/// How long before re-asking about a miss. LRCLIB grows over time, so a track
/// with no lyrics today may well have them in a month.
pub const RETRY_DAYS: [i64; 4] = [7, 30, 90, 180];

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tracks (
    path        TEXT PRIMARY KEY,
    mtime       REAL,
    size        INTEGER,
    artist      TEXT,
    album       TEXT,
    title       TEXT,
    duration    REAL,
    status      TEXT,
    kind        TEXT,
    lrc_path    TEXT,
    lrclib_id   INTEGER,
    score       REAL,
    attempts    INTEGER DEFAULT 0,
    first_seen  TEXT,
    last_attempt TEXT,
    next_retry  TEXT,
    note        TEXT
);
CREATE INDEX IF NOT EXISTS idx_tracks_status ON tracks(status);
CREATE INDEX IF NOT EXISTS idx_tracks_retry  ON tracks(next_retry);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);
"#;

#[derive(Debug, Clone, Default)]
pub struct Row {
    pub path: String,
    pub mtime: f64,
    pub size: i64,
    pub artist: String,
    pub album: String,
    pub title: String,
    pub duration: f64,
    pub status: String,
    pub kind: String,
    pub lrc_path: String,
    pub lrclib_id: Option<i64>,
    pub score: f64,
    pub attempts: i64,
    pub first_seen: String,
    pub last_attempt: String,
    pub next_retry: String,
    pub note: String,
}

/// Which column the track table is sorted by.
///
/// An enum rather than a string because this ends up spliced into SQL: the
/// only orderings that can exist are the ones listed here, so a search box can
/// never reach the ORDER BY clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortCol {
    Status,
    Artist,
    Album,
    Title,
    Attempts,
    Retry,
    Checked,
}

impl SortCol {
    fn column(self) -> &'static str {
        match self {
            SortCol::Status => "status",
            SortCol::Artist => "artist",
            SortCol::Album => "album",
            SortCol::Title => "title",
            SortCol::Attempts => "attempts",
            SortCol::Retry => "next_retry",
            SortCol::Checked => "last_attempt",
        }
    }
    pub const ALL: [SortCol; 7] = [
        SortCol::Status, SortCol::Artist, SortCol::Album, SortCol::Title,
        SortCol::Attempts, SortCol::Retry, SortCol::Checked,
    ];
}

#[derive(Debug, Clone, Copy)]
pub struct Sort {
    pub col: SortCol,
    pub desc: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self { col: SortCol::Artist, desc: false }
    }
}

impl Sort {
    fn sql(&self) -> String {
        let dir = if self.desc { "DESC" } else { "ASC" };
        match self.col {
            // Artist is the natural reading order, so make it the tie-breaker
            // for everything else rather than leaving rows in table order.
            SortCol::Artist => format!("artist {dir}, album ASC, title ASC"),
            c => format!("{} {dir}, artist ASC, album ASC, title ASC", c.column()),
        }
    }
}

pub struct Store {
    pub conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // WAL so the GUI can read while a scan writes - the Python relied on
        // this too, and the database on disk is already in WAL mode.
        conn.pragma_update(None, "journal_mode", "wal").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.pragma_update(None, "busy_timeout", 10_000i64).ok();
        conn.execute_batch(SCHEMA).context("applying schema")?;
        Ok(Self { conn })
    }

    /// Read-only handle for the GUI. Opening the same WAL database twice is
    /// fine; this one simply never writes.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("opening {} read-only", path.display()))?;
        conn.pragma_update(None, "busy_timeout", 5_000i64).ok();
        // 16 MB of page cache rather than SQLite's 2 MB default. The window
        // searches and sorts the whole tracks table, and with the default
        // every one of those re-read most of it from disk.
        conn.pragma_update(None, "cache_size", -16_384i64).ok();
        Ok(Self { conn })
    }

    pub fn get(&self, path: &str) -> Result<Option<Row>> {
        let mut st = self.conn.prepare_cached(
            "SELECT path,mtime,size,artist,album,title,duration,status,kind,lrc_path,\
             lrclib_id,score,attempts,first_seen,last_attempt,next_retry,note \
             FROM tracks WHERE path = ?1",
        )?;
        let row = st
            .query_row(params![path], |r| Ok(Self::row_from(r)))
            .optional()?;
        Ok(row)
    }

    fn row_from(r: &rusqlite::Row<'_>) -> Row {
        Row {
            path: r.get::<_, Option<String>>(0).unwrap_or_default().unwrap_or_default(),
            mtime: r.get::<_, Option<f64>>(1).unwrap_or_default().unwrap_or(0.0),
            size: r.get::<_, Option<i64>>(2).unwrap_or_default().unwrap_or(0),
            artist: r.get::<_, Option<String>>(3).unwrap_or_default().unwrap_or_default(),
            album: r.get::<_, Option<String>>(4).unwrap_or_default().unwrap_or_default(),
            title: r.get::<_, Option<String>>(5).unwrap_or_default().unwrap_or_default(),
            duration: r.get::<_, Option<f64>>(6).unwrap_or_default().unwrap_or(0.0),
            status: r.get::<_, Option<String>>(7).unwrap_or_default().unwrap_or_default(),
            kind: r.get::<_, Option<String>>(8).unwrap_or_default().unwrap_or_default(),
            lrc_path: r.get::<_, Option<String>>(9).unwrap_or_default().unwrap_or_default(),
            lrclib_id: r.get::<_, Option<i64>>(10).unwrap_or_default(),
            score: r.get::<_, Option<f64>>(11).unwrap_or_default().unwrap_or(0.0),
            attempts: r.get::<_, Option<i64>>(12).unwrap_or_default().unwrap_or(0),
            first_seen: r.get::<_, Option<String>>(13).unwrap_or_default().unwrap_or_default(),
            last_attempt: r.get::<_, Option<String>>(14).unwrap_or_default().unwrap_or_default(),
            next_retry: r.get::<_, Option<String>>(15).unwrap_or_default().unwrap_or_default(),
            note: r.get::<_, Option<String>>(16).unwrap_or_default().unwrap_or_default(),
        }
    }

    /// Same retry ladder as the Python: attempt 1 waits 7 days, then 30, 90,
    /// 180, and stays at 180 after that.
    pub fn next_retry_for(status: &str, attempts: i64) -> Option<String> {
        let days = match status {
            ST_NOTFOUND => {
                let idx = (attempts.max(1) - 1).min(RETRY_DAYS.len() as i64 - 1) as usize;
                RETRY_DAYS[idx]
            }
            // A transport failure means we never actually asked, so try again
            // on the very next run rather than parking it for a week.
            ST_ERROR => 0,
            _ => return None,
        };
        Some(iso(Utc::now() + Duration::days(days)))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record(&self, row: &Row) -> Result<()> {
        let existing_first_seen: Option<String> = self
            .conn
            .prepare_cached("SELECT first_seen FROM tracks WHERE path = ?1")?
            .query_row(params![row.path], |r| r.get(0))
            .optional()?
            .flatten();
        let first_seen = existing_first_seen
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| row.first_seen.clone());

        self.conn.prepare_cached(
            "INSERT INTO tracks (path,mtime,size,artist,album,title,duration,status,kind,\
             lrc_path,lrclib_id,score,attempts,first_seen,last_attempt,next_retry,note) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17) \
             ON CONFLICT(path) DO UPDATE SET \
               mtime=excluded.mtime, size=excluded.size, artist=excluded.artist, \
               album=excluded.album, title=excluded.title, duration=excluded.duration, \
               status=excluded.status, kind=excluded.kind, lrc_path=excluded.lrc_path, \
               lrclib_id=excluded.lrclib_id, score=excluded.score, attempts=excluded.attempts, \
               last_attempt=excluded.last_attempt, next_retry=excluded.next_retry, note=excluded.note",
        )?.execute(params![
            row.path, row.mtime, row.size, row.artist, row.album, row.title, row.duration,
            row.status, row.kind, row.lrc_path, row.lrclib_id, row.score, row.attempts,
            first_seen, row.last_attempt,
            if row.next_retry.is_empty() { None } else { Some(row.next_retry.clone()) },
            row.note,
        ])?;
        Ok(())
    }

    pub fn counts(&self) -> Result<HashMap<String, i64>> {
        let mut out = HashMap::new();
        let mut st = self.conn.prepare("SELECT status, COUNT(*) FROM tracks GROUP BY status")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, i64>(1)?))
        })?;
        for r in rows {
            let (s, c) = r?;
            out.insert(s, c);
        }
        Ok(out)
    }

    /// Rows for the GUI's browser: filter by status, free-text search over
    /// artist / album / title / path.
    pub fn browse_sorted(
        &self,
        status: Option<&str>,
        needle: &str,
        limit: usize,
        sort: Sort,
    ) -> Result<Vec<Row>> {
        let like = format!("%{}%", needle.trim());
        let has_needle = !needle.trim().is_empty();
        let sql = format!(
            "SELECT path,mtime,size,artist,album,title,duration,status,kind,lrc_path,\
             lrclib_id,score,attempts,first_seen,last_attempt,next_retry,note FROM tracks \
             WHERE 1=1 {} {} ORDER BY {} LIMIT ?1",
            if status.is_some() { "AND status = ?2" } else { "" },
            if has_needle {
                let n = if status.is_some() { 3 } else { 2 };
                format!(
                    "AND (artist LIKE ?{n} OR album LIKE ?{n} OR title LIKE ?{n} OR path LIKE ?{n})",
                    n = n
                )
            } else {
                String::new()
            },
            sort.sql(),
        );
        let mut st = self.conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row<'_>| Ok(Self::row_from(r));
        let out: Vec<Row> = match (status, has_needle) {
            (Some(s), true) => st
                .query_map(params![limit as i64, s, like], map)?
                .collect::<Result<_, _>>()?,
            (Some(s), false) => st
                .query_map(params![limit as i64, s], map)?
                .collect::<Result<_, _>>()?,
            (None, true) => st
                .query_map(params![limit as i64, like], map)?
                .collect::<Result<_, _>>()?,
            (None, false) => st
                .query_map(params![limit as i64], map)?
                .collect::<Result<_, _>>()?,
        };
        Ok(out)
    }

    pub fn set_meta(&self, k: &str, v: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (k,v) VALUES (?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, k: &str) -> Option<String> {
        self.conn
            .query_row("SELECT v FROM meta WHERE k = ?1", params![k], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// Changes whenever another connection commits - a scan, the hook, a
    /// hand-picked match. The window compares it between refreshes and skips
    /// re-running its queries when nothing in the database has moved.
    pub fn data_version(&self) -> Option<i64> {
        self.conn.query_row("PRAGMA data_version", [], |r| r.get(0)).ok()
    }

    /// `retry --status X`: clear the retry windows so the next scan re-asks.
    pub fn reset_status(&self, status: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE tracks SET next_retry = NULL WHERE status = ?1",
            params![status],
        )?;
        Ok(n)
    }
}

/// RFC3339 with a `+00:00` offset, matching what is already in the table.
pub fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
}

pub fn now_iso() -> String {
    iso(Utc::now())
}

/// Parse a stored timestamp. Tolerant, because a hand-edited or older row
/// should mean "retry now" rather than crashing the scan.
pub fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    if s.trim().is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}
