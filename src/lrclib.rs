//! LRCLIB client: shared rate limit, shared backoff, honest failure reporting.
//!
//! Two details carried over deliberately from the Python, because they are the
//! difference between a useful state database and a poisoned one:
//!
//! * **A 503 slows every worker at once.** LRCLIB is free and donation-funded
//!   and answers `503 ServerOverloaded` under sustained load. One worker
//!   backing off while three others keep hammering just prolongs it, so the
//!   backoff window is shared.
//! * **`is_failure` distinguishes "no lyrics" from "could not ask".** A 404 is
//!   an answer. A timeout, an exhausted 503, or a 200 carrying a truncated
//!   body is not, and must never be recorded as `notfound` - that would park
//!   the track for 7 days over a transient server wobble.

use crate::text::tidy;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RETRYABLE: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];

#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub id: i64,
    pub track_name: String,
    pub artist_name: String,
    pub album_name: String,
    pub duration: f64,
    pub instrumental: bool,
    pub synced: String,
    pub plain: String,
}

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicU64,
    pub retries: AtomicU64,
    pub backoffs: AtomicU64,
}

/// Evenly spaced request slots shared across workers.
struct RateLimiter {
    interval: Duration,
    next: Mutex<Option<Instant>>,
}

impl RateLimiter {
    fn new(rps: f64) -> Self {
        let interval = if rps > 0.0 {
            Duration::from_secs_f64(1.0 / rps)
        } else {
            Duration::ZERO
        };
        Self { interval, next: Mutex::new(None) }
    }
    fn wait(&self) {
        if self.interval.is_zero() {
            return;
        }
        let at = {
            let mut guard = self.next.lock().unwrap();
            let now = Instant::now();
            let at = match *guard {
                Some(n) if n > now => n,
                _ => now,
            };
            *guard = Some(at + self.interval);
            at
        };
        let now = Instant::now();
        if at > now {
            std::thread::sleep(at - now);
        }
    }
}

pub struct Lrclib {
    agent: ureq::Agent,
    host: String,
    max_retries: u32,
    limiter: RateLimiter,
    backoff_until: Mutex<Option<Instant>>,
    pub stats: Arc<Stats>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Lrclib {
    pub fn new(cfg: &crate::config::Lrclib, stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs_f64(cfg.timeout_seconds))
            .user_agent(concat!(
                "lyricarr/", env!("CARGO_PKG_VERSION"),
                " (self-hosted library tool)"
            ))
            .build();
        Self {
            agent,
            host: cfg.host.clone(),
            max_retries: cfg.max_retries,
            limiter: RateLimiter::new(cfg.requests_per_second),
            backoff_until: Mutex::new(None),
            stats: Arc::new(Stats::default()),
            stop,
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn hold(&self, secs: f64) {
        let until = Instant::now() + Duration::from_secs_f64(secs.min(60.0));
        let mut g = self.backoff_until.lock().unwrap();
        if g.map_or(true, |cur| until > cur) {
            *g = Some(until);
        }
        self.stats.backoffs.fetch_add(1, Ordering::Relaxed);
    }

    fn await_backoff(&self) {
        loop {
            if self.stopping() {
                return;
            }
            let remaining = {
                let g = self.backoff_until.lock().unwrap();
                match *g {
                    Some(t) => t.checked_duration_since(Instant::now()),
                    None => None,
                }
            };
            match remaining {
                Some(d) if !d.is_zero() => std::thread::sleep(d.min(Duration::from_secs(1))),
                _ => return,
            }
        }
    }

    /// `(status, parsed)`. Status 0 means "never got an answer".
    fn get_json(&self, path_and_query: &str) -> (u16, Option<Value>) {
        let url = format!("https://{}{}", self.host, path_and_query);
        let mut delay = 1.0f64;
        for attempt in 0..=self.max_retries {
            if self.stopping() {
                return (0, None);
            }
            self.await_backoff();
            self.limiter.wait();

            let res = self.agent.get(&url).set("Accept", "application/json").call();
            match res {
                Ok(resp) => {
                    self.stats.requests.fetch_add(1, Ordering::Relaxed);
                    let status = resp.status();
                    match resp.into_json::<Value>() {
                        Ok(v) => return (status, Some(v)),
                        Err(_) => {
                            // 200 but truncated or not JSON: a transport
                            // problem, not an answer.
                            return (0, None);
                        }
                    }
                }
                Err(ureq::Error::Status(status, resp)) => {
                    self.stats.requests.fetch_add(1, Ordering::Relaxed);
                    if status == 404 {
                        return (404, None);
                    }
                    if RETRYABLE.contains(&status) {
                        let mut wait = delay;
                        if let Some(ra) = resp.header("Retry-After").and_then(|h| h.parse::<f64>().ok()) {
                            wait = wait.max(ra);
                        }
                        self.hold(wait);
                        self.stats.retries.fetch_add(1, Ordering::Relaxed);
                        if attempt >= self.max_retries {
                            return (status, None);
                        }
                        delay = (delay * 2.0).min(30.0);
                        continue;
                    }
                    return (status, None);
                }
                Err(_transport) => {
                    self.stats.retries.fetch_add(1, Ordering::Relaxed);
                    if attempt >= self.max_retries {
                        return (0, None);
                    }
                    std::thread::sleep(Duration::from_secs_f64(delay));
                    delay = (delay * 2.0).min(30.0);
                    continue;
                }
            }
        }
        (0, None)
    }

    /// True when we never got a usable answer, so the track must NOT be
    /// recorded as having no lyrics.
    pub fn is_failure(status: u16) -> bool {
        status == 0 || RETRYABLE.contains(&status)
    }

    /// Skip empty values the way the Python's `_q` did, so an absent album or
    /// an unknown duration is simply left out of the query.
    fn query(pairs: &[(&str, String)]) -> String {
        let mut out = String::new();
        for (k, v) in pairs {
            if v.is_empty() || v == "0" {
                continue;
            }
            if !out.is_empty() {
                out.push('&');
            }
            out.push_str(k);
            out.push('=');
            out.push_str(&urlencode(v));
        }
        out
    }

    /// `(candidate, hard_failure)`
    pub fn api_get(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration: f64,
    ) -> (Option<Candidate>, bool) {
        let dur = if duration > 0.0 {
            format!("{}", duration.round() as i64)
        } else {
            String::new()
        };
        let q = Self::query(&[
            ("artist_name", artist.to_string()),
            ("track_name", title.to_string()),
            ("album_name", album.to_string()),
            ("duration", dur),
        ]);
        let (status, data) = self.get_json(&format!("/api/get?{}", q));
        if status == 200 {
            if let Some(v) = data.as_ref().filter(|v| v.is_object()) {
                return (to_candidate(v), false);
            }
        }
        (None, Self::is_failure(status))
    }

    /// `(candidates, hard_failure)`
    pub fn api_search(&self, artist: &str, title: &str) -> (Vec<Candidate>, bool) {
        let q = Self::query(&[
            ("artist_name", artist.to_string()),
            ("track_name", title.to_string()),
        ]);
        let (status, data) = self.get_json(&format!("/api/search?{}", q));
        if status == 200 {
            if let Some(arr) = data.as_ref().and_then(|v| v.as_array()) {
                return (arr.iter().filter_map(to_candidate).collect(), false);
            }
        }
        (Vec::new(), Self::is_failure(status))
    }
}

fn to_candidate(v: &Value) -> Option<Candidate> {
    let o = v.as_object()?;
    let s = |k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let name = {
        let t = s("trackName");
        if t.is_empty() { s("name") } else { t }
    };
    Some(Candidate {
        id: o.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
        track_name: tidy(&name),
        artist_name: tidy(&s("artistName")),
        album_name: tidy(&s("albumName")),
        duration: o.get("duration").and_then(|x| x.as_f64()).unwrap_or(0.0),
        instrumental: o.get("instrumental").and_then(|x| x.as_bool()).unwrap_or(false),
        synced: o.get("syncedLyrics").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        plain: o.get("plainLyrics").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

/// Percent-encoding for query values. `urlencode` in the Python encoded spaces
/// as `+`; LRCLIB accepts either, and `%20` is unambiguous.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
