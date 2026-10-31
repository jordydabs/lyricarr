//! The jobs automation cannot finish for you.
//!
//! A good share of a typical library comes back `notfound`, and the misses
//! are overwhelmingly genuine absences from LRCLIB rather than matcher failures.
//! For the rest - a spelling the tags disagree on, a release LRCLIB files under
//! another title - no amount of scoring will help, but a person looking at
//! three candidates will get it right in two seconds. That is what
//! `manual_search` / `apply_match` are for.
//!
//! `find_strays` exists because lyrics files left behind by other tools often
//! sit beside no matching audio file at all. Orphans like that are invisible:
//! no player shows them, and nothing in a scan looks for them.

use crate::config::Config;
use crate::lrclib::{Candidate, Lrclib};
use crate::store::{self, Row, Store};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Free-text LRCLIB search for the manual override dialog.
pub fn manual_search(cfg: &Config, artist: &str, title: &str) -> Vec<Candidate> {
    let stop = Arc::new(AtomicBool::new(false));
    let client = Lrclib::new(&cfg.lrclib, stop);
    let (mut cands, _failed) = client.api_search(artist, title);
    // Most useful first: synced before plain, then longer text.
    cands.sort_by(|a, b| {
        let ka = (!a.synced.trim().is_empty(), a.synced.len() + a.plain.len());
        let kb = (!b.synced.trim().is_empty(), b.synced.len() + b.plain.len());
        kb.cmp(&ka)
    });
    cands
}

/// Write a chosen candidate's lyrics for a track and record it, exactly as a
/// scan would - same sidecar rules, same database columns - so a later scan
/// sees it as done and does not undo the choice.
pub fn apply_match(cfg: &Config, audio: &Path, cand: &Candidate) -> Result<PathBuf> {
    let kind = if !cand.synced.trim().is_empty() {
        "synced"
    } else {
        "plain"
    };
    let text = if kind == "synced" { &cand.synced } else { &cand.plain };
    let dest = crate::scan::sidecar_for(audio, kind, cfg)
        .ok_or_else(|| anyhow::anyhow!("output.plain_lyrics = \"skip\", so there is nowhere to write unsynced lyrics"))?;

    let body = {
        let mut s = text.replace("\r\n", "\n");
        while s.ends_with('\n') {
            s.pop();
        }
        s.push('\n');
        s
    };
    let tmp = dest.with_extension("lyricarr.tmp");
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, &dest)?;

    let md = std::fs::metadata(audio)?;
    let mtime = md
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let tr = crate::tags::read_track(audio, mtime, md.len() as i64);
    let now = store::now_iso();
    let s = Store::open(Path::new(&cfg.run.database))?;
    let attempts = s.get(&tr.path)?.map(|r| r.attempts).unwrap_or(0) + 1;
    s.record(&Row {
        path: audio.to_string_lossy().to_string(),
        mtime,
        size: md.len() as i64,
        artist: tr.search_artist().to_string(),
        album: tr.album.clone(),
        title: tr.title.clone(),
        duration: tr.duration,
        status: (if kind == "synced" { store::ST_SYNCED } else { store::ST_PLAIN }).to_string(),
        kind: kind.into(),
        lrc_path: dest.to_string_lossy().to_string(),
        lrclib_id: if cand.id != 0 { Some(cand.id) } else { None },
        score: 1.0,
        attempts,
        first_seen: now.clone(),
        last_attempt: now,
        next_retry: String::new(),
        // Marked so it is obvious later that a person chose this, not the scorer.
        note: "chosen by hand".into(),
    })?;
    Ok(dest)
}

/// Record a track as instrumental so it is never queried again - for the ones
/// LRCLIB has no entry for but you know have no vocals.
pub fn mark_instrumental(cfg: &Config, audio: &Path) -> Result<()> {
    let md = std::fs::metadata(audio)?;
    let mtime = md.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs_f64();
    let tr = crate::tags::read_track(audio, mtime, md.len() as i64);
    let now = store::now_iso();
    let s = Store::open(Path::new(&cfg.run.database))?;
    let attempts = s.get(&tr.path)?.map(|r| r.attempts).unwrap_or(0) + 1;
    s.record(&Row {
        path: audio.to_string_lossy().to_string(),
        mtime,
        size: md.len() as i64,
        artist: tr.search_artist().to_string(),
        album: tr.album.clone(),
        title: tr.title.clone(),
        duration: tr.duration,
        status: store::ST_INSTRUMENTAL.into(),
        kind: String::new(),
        lrc_path: String::new(),
        lrclib_id: None,
        score: 1.0,
        attempts,
        first_seen: now.clone(),
        last_attempt: now,
        next_retry: String::new(),
        note: "marked instrumental by hand".into(),
    })?;
    Ok(())
}

/// Forget a track entirely, so the next scan treats it as new.
pub fn forget(cfg: &Config, audio: &Path) -> Result<()> {
    let s = Store::open(Path::new(&cfg.run.database))?;
    s.conn.execute(
        "DELETE FROM tracks WHERE path = ?1",
        rusqlite::params![audio.to_string_lossy().to_string()],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Stray {
    pub lrc: PathBuf,
    pub bytes: u64,
}

/// Sidecars with no audio file of the same stem beside them. They are dead
/// weight: no player will ever show them, and nothing in a scan looks for them.
///
/// Only the extensions Lyricarr itself writes are considered. Sweeping every
/// `.txt` would flag album notes and `folder.txt` as orphans, and a tool that
/// offers to delete things has to be right about what it found.
pub fn find_strays(cfg: &Config, limit: usize, stop: &AtomicBool) -> Vec<Stray> {
    use std::sync::atomic::Ordering;
    let mut exts: Vec<&str> = vec!["lrc"];
    if cfg.output.plain_lyrics.eq_ignore_ascii_case("txt") {
        exts.push("txt");
    }
    let mut out = Vec::new();
    for root in &cfg.library.roots {
        for entry in jwalk::WalkDir::new(root).skip_hidden(false) {
            if out.len() >= limit || stop.load(Ordering::Relaxed) {
                return out;
            }
            let Ok(e) = entry else { continue };
            if e.file_type().is_dir() {
                continue;
            }
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            if !exts.contains(&ext.as_str()) {
                continue;
            }
            if p.components().any(|c| cfg.skips_dir(&c.as_os_str().to_string_lossy())) {
                continue;
            }
            let stem = p.with_extension("");
            let has_audio = cfg
                .library
                .audio_extensions
                .iter()
                .any(|ext| stem.with_extension(ext.trim_start_matches('.')).is_file());
            if !has_audio {
                let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(Stray { lrc: p, bytes });
            }
        }
    }
    out
}

/// Remove one stray. Deliberately per-file and called from a confirmed button
/// rather than a sweep: these sit in somebody's music folders.
pub fn delete_stray(p: &Path) -> std::io::Result<()> {
    std::fs::remove_file(p)
}

#[derive(Debug, Clone)]
pub struct ArtistCoverage {
    pub artist: String,
    pub total: i64,
    pub done: i64,
}

impl ArtistCoverage {
    pub fn pct(&self) -> f64 {
        if self.total == 0 { 0.0 } else { 100.0 * self.done as f64 / self.total as f64 }
    }
}

/// Which artists are worst covered, so a `--path` run can be aimed where it
/// will actually pay. Only artists with enough tracks to be meaningful.
pub fn artist_coverage(db: &Store, min_tracks: i64, limit: usize) -> Result<Vec<ArtistCoverage>> {
    let mut st = db.conn.prepare(
        "SELECT COALESCE(NULLIF(artist,''),'(no artist)') AS a, \
                COUNT(*) AS n, \
                SUM(CASE WHEN status IN ('synced','plain','instrumental') THEN 1 ELSE 0 END) AS d \
         FROM tracks GROUP BY a HAVING n >= ?1 ORDER BY (CAST(d AS REAL)/n) ASC, n DESC LIMIT ?2",
    )?;
    let rows = st.query_map(rusqlite::params![min_tracks, limit as i64], |r| {
        Ok(ArtistCoverage {
            artist: r.get::<_, String>(0)?,
            total: r.get::<_, i64>(1)?,
            done: r.get::<_, i64>(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Read a sidecar back so it can be shown (and sanity-checked) in the window.
pub fn read_sidecar(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// Where the lyrics for this track live right now, if anywhere.
pub fn sidecar_of(audio: &Path) -> Option<PathBuf> {
    crate::scan::existing_sidecars(audio).into_iter().next()
}
