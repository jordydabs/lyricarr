//! Deciding which LRCLIB entry is this track.
//!
//! Accuracy matters more than coverage: a wrong `.lrc` silently desyncs during
//! playback and nobody reports it, whereas a miss is just a miss. So the
//! ladder is deliberately conservative, and it is a faithful port of the
//! Python - the thresholds were calibrated against a real library, and the
//! similarity function they were calibrated with is reproduced exactly in
//! `crate::text`.
//!
//! 1. `/api/get` on up to three progressively simpler variants. Most tracks
//!    hit on the first request.
//! 2. `/api/search`, scored on duration (45%), title (35%) and artist (20%).
//!    Candidates outside the duration window are rejected outright - search
//!    will happily return a 32-second "Get Lucky", and length is the strongest
//!    signal that two entries are the same recording.
//! 3. Relaxed, **unsynced only**. Plain lyrics carry no timestamps, so a radio
//!    edit or live cut of a different length still has the right words. Synced
//!    lyrics are never accepted this way.

use crate::config::Config;
use crate::lrclib::{Candidate, Lrclib};
use crate::store::{ST_ERROR, ST_INSTRUMENTAL, ST_NOTFOUND, ST_PLAIN, ST_SYNCED};
use crate::tags::Track;
use crate::text::{
    norm_key, primary_artist, ratio, strip_all_brackets, strip_feat, strip_noise_brackets, tidy,
};

pub struct Match {
    pub status: &'static str,
    pub candidate: Option<Candidate>,
    pub kind: String,
    pub text: String,
    pub score: f64,
    pub note: String,
    pub requests: u32,
}

pub struct Matcher<'a> {
    cfg: &'a Config,
    client: &'a Lrclib,
}

impl<'a> Matcher<'a> {
    pub fn new(cfg: &'a Config, client: &'a Lrclib) -> Self {
        Self { cfg, client }
    }

    /// `(artist, title, album)` attempts, most precise first, de-duplicated.
    pub fn variants(&self, tr: &Track) -> Vec<(String, String, String)> {
        let artist = tr.search_artist().to_string();

        let mut titles: Vec<String> = Vec::new();
        for t in [
            tr.title.clone(),
            strip_feat(&tr.title),
            strip_noise_brackets(&tr.title),
            strip_all_brackets(&strip_feat(&tr.title)),
        ] {
            let t = tidy(&t);
            if !t.is_empty() && !titles.contains(&t) {
                titles.push(t);
            }
        }
        let mut artists: Vec<String> = Vec::new();
        for a in [
            artist.clone(),
            strip_feat(&artist),
            primary_artist(&artist),
            tr.albumartist.clone(),
            primary_artist(&tr.albumartist),
        ] {
            let a = tidy(&a);
            if !a.is_empty() && !artists.contains(&a) {
                artists.push(a);
            }
        }
        if titles.is_empty() || artists.is_empty() {
            return Vec::new();
        }

        let mut out: Vec<(String, String, String)> = Vec::new();
        let mut seen: Vec<(String, String, String)> = Vec::new();
        let mut add = |a: &str, t: &str, al: &str| {
            if a.is_empty() || t.is_empty() {
                return;
            }
            let key = (norm_key(a), norm_key(t), norm_key(al));
            if seen.contains(&key) {
                return;
            }
            seen.push(key);
            out.push((a.to_string(), t.to_string(), al.to_string()));
        };

        // 1. exactly what the tags say, album included
        add(&artists[0], &titles[0], &tr.album);
        // 2. same, without the album (album naming differs a lot between releases)
        add(&artists[0], &titles[0], "");
        // 3. cleaned title / primary artist
        for t in titles.iter().skip(1) {
            add(&artists[0], t, "");
        }
        for a in artists.iter().skip(1) {
            add(a, &titles[0], "");
        }
        if artists.len() > 1 && titles.len() > 1 {
            add(&artists[1], &titles[1], "");
        }
        out
    }

    fn score(&self, cand: &Candidate, tr: &Track) -> f64 {
        let c = &self.cfg.lrclib;
        let mut t_ratio = ratio(&norm_key(&cand.track_name), &norm_key(&tr.title));
        if t_ratio < c.min_title_ratio {
            let alt = ratio(&norm_key(&cand.track_name), &norm_key(&strip_feat(&tr.title)));
            t_ratio = t_ratio.max(alt);
        }
        let a_ratio = [
            ratio(&norm_key(&cand.artist_name), &norm_key(tr.search_artist())),
            ratio(
                &norm_key(&cand.artist_name),
                &norm_key(&primary_artist(tr.search_artist())),
            ),
            if tr.albumartist.is_empty() {
                0.0
            } else {
                ratio(&norm_key(&cand.artist_name), &norm_key(&tr.albumartist))
            },
        ]
        .into_iter()
        .fold(0.0f64, f64::max);

        let d_score = if tr.duration > 0.0 && cand.duration > 0.0 {
            let delta = (cand.duration - tr.duration).abs();
            if delta > c.duration_tolerance_search {
                return 0.0;
            }
            1.0 - (delta / c.duration_tolerance_search)
        } else {
            // unknown duration: neither reward nor punish
            0.5
        };

        let mut score = 0.45 * d_score + 0.35 * t_ratio + 0.20 * a_ratio;
        if t_ratio < c.min_title_ratio {
            score *= 0.5;
        }
        score
    }

    /// What this candidate can actually give us: `("", "")` means nothing
    /// usable (empty payload, or shorter than `min_lyrics_chars`).
    fn usable(&self, cand: &Candidate) -> (&'static str, String) {
        let min = self.cfg.output.min_lyrics_chars;
        if cand.instrumental {
            return ("instrumental", String::new());
        }
        if !cand.synced.trim().is_empty() && cand.synced.trim().len() >= min {
            return ("synced", cand.synced.clone());
        }
        if !cand.plain.trim().is_empty() && cand.plain.trim().len() >= min {
            return ("plain", cand.plain.clone());
        }
        ("", String::new())
    }

    fn text_ratios(&self, cand: &Candidate, tr: &Track) -> (f64, f64) {
        let t = [
            ratio(&norm_key(&cand.track_name), &norm_key(&tr.title)),
            ratio(&norm_key(&cand.track_name), &norm_key(&strip_feat(&tr.title))),
            ratio(
                &norm_key(&cand.track_name),
                &norm_key(&strip_noise_brackets(&tr.title)),
            ),
        ]
        .into_iter()
        .fold(0.0f64, f64::max);
        let a = [
            ratio(&norm_key(&cand.artist_name), &norm_key(tr.search_artist())),
            ratio(
                &norm_key(&cand.artist_name),
                &norm_key(&primary_artist(tr.search_artist())),
            ),
            if tr.albumartist.is_empty() {
                0.0
            } else {
                ratio(&norm_key(&cand.artist_name), &norm_key(&tr.albumartist))
            },
        ]
        .into_iter()
        .fold(0.0f64, f64::max);
        (t, a)
    }

    pub fn find(&self, tr: &Track) -> Match {
        let c = &self.cfg.lrclib;
        let attempts = self.variants(tr);
        let mut used = 0u32;
        let mut failed = false;

        // Stage 1: /api/get on the precise variants. Cheap and exact.
        for (artist, title, album) in attempts.iter().take(3) {
            let (cand, hard) = self.client.api_get(artist, title, album, tr.duration);
            used += 1;
            failed |= hard;
            if let Some(cand) = cand {
                let (kind, text) = self.usable(&cand);
                if kind == "instrumental" {
                    return Match {
                        status: ST_INSTRUMENTAL,
                        candidate: Some(cand),
                        kind: String::new(),
                        text: String::new(),
                        score: 1.0,
                        note: "api/get".into(),
                        requests: used,
                    };
                }
                if !kind.is_empty() {
                    return Match {
                        status: if kind == "synced" { ST_SYNCED } else { ST_PLAIN },
                        candidate: Some(cand),
                        kind: kind.to_string(),
                        text,
                        score: 1.0,
                        note: "api/get".into(),
                        requests: used,
                    };
                }
            }
        }

        // Stage 2: /api/search, then score. Catches tag/title drift.
        let (cands, hard) = match attempts.first() {
            Some((artist, title, _)) => {
                let r = self
                    .client
                    .api_search(&primary_artist(artist), &strip_feat(title));
                used += 1;
                r
            }
            None => (Vec::new(), false),
        };
        failed |= hard;

        let mut best: Option<&Candidate> = None;
        let mut best_score = 0.0f64;
        let mut best_kind = String::new();
        let mut best_text = String::new();
        for cand in &cands {
            let sc = self.score(cand, tr);
            if sc < c.min_score || sc <= best_score {
                continue;
            }
            let (kind, text) = self.usable(cand);
            if kind.is_empty() {
                continue;
            }
            // prefer synced: a plain hit only wins if clearly better
            if best_kind == "synced" && kind == "plain" && sc < best_score + 0.08 {
                continue;
            }
            best = Some(cand);
            best_score = sc;
            best_kind = kind.to_string();
            best_text = text;
        }
        if let Some(b) = best {
            if best_kind == "instrumental" {
                return Match {
                    status: ST_INSTRUMENTAL,
                    candidate: Some(b.clone()),
                    kind: String::new(),
                    text: String::new(),
                    score: best_score,
                    note: "api/search".into(),
                    requests: used,
                };
            }
            return Match {
                status: if best_kind == "synced" { ST_SYNCED } else { ST_PLAIN },
                candidate: Some(b.clone()),
                kind: best_kind,
                text: best_text,
                score: best_score,
                note: "api/search".into(),
                requests: used,
            };
        }

        // Stage 3: relaxed, plain text only. The strict gate above protects
        // synced timings; unsynced words are still correct for a release of a
        // different length, so accept them when the title and artist match
        // closely and the duration is merely in the same ballpark.
        let mut relaxed: Option<&Candidate> = None;
        let mut relaxed_t = 0.0f64;
        for cand in &cands {
            if cand.instrumental {
                continue;
            }
            if cand.plain.trim().len() < self.cfg.output.min_lyrics_chars {
                continue;
            }
            let (t_ratio, a_ratio) = self.text_ratios(cand, tr);
            if t_ratio < c.plain_min_title_ratio || a_ratio < c.plain_min_artist_ratio {
                continue;
            }
            if tr.duration > 0.0
                && cand.duration > 0.0
                && (cand.duration - tr.duration).abs() > c.plain_duration_tolerance
            {
                continue;
            }
            if t_ratio > relaxed_t {
                relaxed = Some(cand);
                relaxed_t = t_ratio;
            }
        }
        if let Some(r) = relaxed {
            return Match {
                status: ST_PLAIN,
                candidate: Some(r.clone()),
                kind: "plain".into(),
                text: r.plain.clone(),
                score: relaxed_t,
                note: "relaxed/plain".into(),
                requests: used,
            };
        }

        // Nothing matched. Say *why*: an unreachable server must not be
        // remembered as "this track has no lyrics".
        if failed {
            Match {
                status: ST_ERROR,
                candidate: None,
                kind: String::new(),
                text: String::new(),
                score: 0.0,
                note: "lookup failed (server/network)".into(),
                requests: used,
            }
        } else {
            Match {
                status: ST_NOTFOUND,
                candidate: None,
                kind: String::new(),
                text: String::new(),
                score: 0.0,
                note: "no match".into(),
                requests: used,
            }
        }
    }
}
