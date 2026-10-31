//! String normalisation and similarity.
//!
//! This is the one module that has to be a *bit-for-bit* port rather than a
//! reimplementation. The matcher's thresholds (`min_score` 0.80,
//! `min_title_ratio` 0.72, `plain_min_title_ratio` 0.90) were calibrated
//! against Python's `difflib.SequenceMatcher.ratio()` on a real library. Swapping
//! in Levenshtein or Jaro-Winkler would keep the numbers and silently change
//! which tracks match - the worst kind of regression here, because a wrong
//! `.lrc` desyncs during playback and nobody reports it.
//!
//! So `ratio()` below is Ratcliff-Obershelp exactly as CPython implements it,
//! including the `autojunk` heuristic and the junk-extension passes, and
//! `tests::ratio_matches_python` checks it against vectors generated from the
//! real `difflib` (see `ratio_vectors.json`).

use std::collections::HashMap;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

// ---------------------------------------------------------------------------
// difflib.SequenceMatcher.ratio()
// ---------------------------------------------------------------------------

/// Longest matching block in `a[alo..ahi]` / `b[blo..bhi]`, preferring the
/// earliest in `a`, then the earliest in `b` - CPython's tie-breaking.
fn find_longest_match(
    a: &[char],
    b: &[char],
    b2j: &HashMap<char, Vec<usize>>,
    bjunk: &dyn Fn(char) -> bool,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
    // j2len[j] = length of the longest match ending at a[i-1], b[j-1]
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for i in alo..ahi {
        let mut newj2len: HashMap<usize, usize> = HashMap::new();
        if let Some(js) = b2j.get(&a[i]) {
            for &j in js {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j.checked_sub(1).and_then(|jm| j2len.get(&jm)).copied().unwrap_or(0) + 1;
                newj2len.insert(j, k);
                if k > bestsize {
                    besti = i + 1 - k;
                    bestj = j + 1 - k;
                    bestsize = k;
                }
            }
        }
        j2len = newj2len;
    }

    // Extend by non-junk on each end...
    while besti > alo
        && bestj > blo
        && !bjunk(b[bestj - 1])
        && a[besti - 1] == b[bestj - 1]
    {
        besti -= 1;
        bestj -= 1;
        bestsize += 1;
    }
    while besti + bestsize < ahi
        && bestj + bestsize < bhi
        && !bjunk(b[bestj + bestsize])
        && a[besti + bestsize] == b[bestj + bestsize]
    {
        bestsize += 1;
    }
    // ...then suck up the matching junk beside it too.
    while besti > alo && bestj > blo && bjunk(b[bestj - 1]) && a[besti - 1] == b[bestj - 1] {
        besti -= 1;
        bestj -= 1;
        bestsize += 1;
    }
    while besti + bestsize < ahi
        && bestj + bestsize < bhi
        && bjunk(b[bestj + bestsize])
        && a[besti + bestsize] == b[bestj + bestsize]
    {
        bestsize += 1;
    }

    (besti, bestj, bestsize)
}

/// `difflib.SequenceMatcher(None, a, b).ratio()`.
///
/// Operates on `char`s, not bytes: music libraries are full of accented artist
/// names and a byte-wise port would score "Sigur Rós" against "Sigur Ros"
/// differently from the Python it is replacing.
pub fn ratio(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();

    // b2j, with CPython's autojunk: for sequences of 200+ elements, anything
    // appearing in more than 1% of b is treated as junk ("popular"). Titles and
    // artists never reach 200 characters, so this is here for exactness rather
    // than effect.
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (j, &ch) in bv.iter().enumerate() {
        b2j.entry(ch).or_default().push(j);
    }
    let n = bv.len();
    let mut popular: Vec<char> = Vec::new();
    if n >= 200 {
        let ntest = n / 100 + 1;
        for (ch, js) in b2j.iter() {
            if js.len() > ntest {
                popular.push(*ch);
            }
        }
        for ch in &popular {
            b2j.remove(ch);
        }
    }
    let bjunk = move |c: char| popular.contains(&c);

    // get_matching_blocks(), summing block sizes. The real function also merges
    // adjacent blocks, which cannot change the total, so ratio() does not care.
    let mut matches = 0usize;
    let mut queue = vec![(0usize, av.len(), 0usize, bv.len())];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = find_longest_match(&av, &bv, &b2j, &bjunk, alo, ahi, blo, bhi);
        if k > 0 {
            matches += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    2.0 * matches as f64 / (av.len() + bv.len()) as f64
}

// ---------------------------------------------------------------------------
// normalisation
// ---------------------------------------------------------------------------

/// The same substitutions `_PUNCT_MAP` makes: curly quotes and the various
/// dashes folded to ASCII, non-breaking space to a space, zero-width and BOM
/// dropped.
fn punct_fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{02BC}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{2013}' | '\u{2014}' | '\u{2012}' | '\u{2212}' => out.push('-'),
            '\u{00A0}' => out.push(' '),
            '\u{200B}' | '\u{FEFF}' => {}
            _ => out.push(c),
        }
    }
    out
}

fn squeeze_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

/// Light cleanup that keeps the string human-readable (`tidy`).
pub fn tidy(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let nfc: String = s.nfc().collect();
    squeeze_ws(&punct_fold(&nfc))
}

/// Aggressive form used only for similarity comparison (`norm_key`):
/// casefold, decompose, drop combining marks, drop non-word characters.
pub fn norm_key(s: &str) -> String {
    let t = tidy(s).to_lowercase();
    let decomposed: String = t.nfkd().filter(|c| !is_combining_mark(*c)).collect();
    // Python does `re.sub(r"[^\w\s]", "", s)` - it DELETES punctuation rather
    // than standing a space in its place. Substituting a space instead scored
    // "AC/DC" as "ac dc" against LRCLIB's "acdc", and "Jay-Z" as "jay z":
    // both then fell under min_title_ratio and lost matches that the Python
    // found. Caught by normalisers_match_python.
    let kept: String = decomposed
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || c.is_whitespace())
        .collect();
    squeeze_ws(&kept)
}

const NOISE_WORDS: &[&str] = &[
    "remaster", "remastered", "original mix", "radio edit", "album version",
    "single version", "explicit", "clean", "bonus track", "deluxe", "mono",
    "stereo", "reissue", "anniversary", "digital remaster", "edit",
];

const FEAT_WORDS: &[&str] = &["feat.", "feat", "featuring", "ft.", "ft", "with"];

/// Spans of `(...)` and `[...]` in `s`, as (start, end, inner) char-index triples.
fn bracket_spans(s: &str) -> Vec<(usize, usize, String)> {
    let v: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < v.len() {
        let open = v[i];
        if open == '(' || open == '[' {
            let close = if open == '(' { ')' } else { ']' };
            if let Some(off) = v[i + 1..].iter().position(|&c| c == close) {
                let end = i + 1 + off;
                let inner: String = v[i + 1..end].iter().collect();
                // include the run of whitespace before the bracket, as the
                // Python pattern's leading \s* does
                let mut start = i;
                while start > 0 && v[start - 1].is_whitespace() {
                    start -= 1;
                }
                out.push((start, end + 1, inner));
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn drop_spans(s: &str, spans: &[(usize, usize)]) -> String {
    let v: Vec<char> = s.chars().collect();
    let mut keep = vec![true; v.len()];
    for &(a, b) in spans {
        for k in a..b.min(v.len()) {
            keep[k] = false;
        }
    }
    v.iter()
        .zip(keep)
        .filter_map(|(c, k)| if k { Some(*c) } else { None })
        .collect()
}

fn word_at(hay: &str, needle: &str) -> bool {
    let h = format!(" {} ", norm_key(hay));
    let n = norm_key(needle);
    !n.is_empty() && h.contains(&format!(" {} ", n))
}

/// `strip_feat`: drop an inline `(feat. …)` / `[ft …]`, and a trailing
/// `feat …` / `with …` tail.
pub fn strip_feat(s: &str) -> String {
    // inline bracketed feature credit
    let spans: Vec<(usize, usize)> = bracket_spans(s)
        .into_iter()
        .filter(|(_, _, inner)| {
            let first = inner.trim().split_whitespace().next().unwrap_or("").to_lowercase();
            let first = first.trim_end_matches('.').to_string();
            matches!(first.as_str(), "feat" | "featuring" | "ft")
        })
        .map(|(a, b, _)| (a, b))
        .collect();
    let mut out = drop_spans(s, &spans);

    // trailing credit: "... feat X", "... (with Y", "... ft. Z)"
    let lower = out.to_lowercase();
    let mut cut: Option<usize> = None;
    for w in FEAT_WORDS {
        let pat = format!(" {} ", w);
        if let Some(p) = lower.rfind(&pat) {
            // only treat it as a trailing credit, i.e. nothing but the credit after it
            let tail = &out[p + pat.len()..];
            if !tail.contains(" - ") {
                cut = Some(cut.map_or(p, |c: usize| c.min(p)));
            }
        }
        // bracket-opened credit with no close, e.g. "Song (feat. X"
        for ob in ['(', '['] {
            let pat2 = format!("{}{} ", ob, w);
            if let Some(p) = lower.rfind(&pat2) {
                cut = Some(cut.map_or(p, |c: usize| c.min(p)));
            }
        }
    }
    if let Some(p) = cut {
        out = out[..p].to_string();
    }
    tidy(&out)
}

/// `strip_noise_brackets`: drop `(Remastered)`, `[Explicit]`, `(Original Mix)`
/// and a trailing ` - Remastered`, but keep meaningful parentheses like
/// `(Live)`.
pub fn strip_noise_brackets(s: &str) -> String {
    let spans: Vec<(usize, usize)> = bracket_spans(s)
        .into_iter()
        .filter(|(_, _, inner)| {
            let il = inner.to_lowercase();
            NOISE_WORDS.iter().any(|w| il.contains(w))
        })
        .map(|(a, b, _)| (a, b))
        .collect();
    let mut out = drop_spans(s, &spans);

    // trailing " - <noise>" with no further dash after it
    if let Some(p) = out.rfind(" - ") {
        let tail = &out[p + 3..];
        if !tail.contains('-') && NOISE_WORDS.iter().any(|w| word_at(tail, w)) {
            out = out[..p].to_string();
        }
    }
    tidy(&out)
}

/// `strip_all_brackets`: every `(...)` / `[...]` removed.
pub fn strip_all_brackets(s: &str) -> String {
    let spans: Vec<(usize, usize)> = bracket_spans(s).into_iter().map(|(a, b, _)| (a, b)).collect();
    tidy(&drop_spans(s, &spans))
}

/// `primary_artist`: first credited artist. "A; B" -> "A". Guards against band
/// names that legitimately contain a separator by requiring the left side to be
/// at least two characters and there to be a real right side.
pub fn primary_artist(s: &str) -> String {
    let t = tidy(s);
    if t.is_empty() {
        return String::new();
    }
    let v: Vec<char> = t.chars().collect();
    // separators: ; / | & + ,  and the words "vs", "vs.", "and"
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0usize;
    while i < v.len() {
        let c = v[i];
        let two: String = v[i..(i + 4).min(v.len())].iter().collect();
        let low = two.to_lowercase();
        let word_sep = if low.starts_with("vs.") && i + 3 <= v.len() {
            Some(3)
        } else if low.starts_with("vs") && (i + 2 >= v.len() || v[i + 2].is_whitespace()) {
            Some(2)
        } else if low.starts_with("and") && (i + 3 >= v.len() || v[i + 3].is_whitespace()) {
            Some(3)
        } else {
            None
        };
        let boundary_ok = i == 0 || v[i - 1].is_whitespace();
        if let Some(len) = word_sep {
            if boundary_ok {
                parts.push(cur.trim().to_string());
                cur.clear();
                i += len;
                continue;
            }
        }
        if matches!(c, ';' | '/' | '|' | '&' | '+' | ',') {
            parts.push(cur.trim().to_string());
            cur.clear();
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    parts.push(cur.trim().to_string());
    let parts: Vec<String> = parts.into_iter().filter(|p| !p.trim().is_empty()).collect();
    if parts.len() >= 2 && parts[0].chars().count() >= 2 {
        return parts[0].trim().to_string();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct RatioVec {
        a: String,
        b: String,
        ratio: f64,
    }

    /// The whole point of this module: identical numbers to Python's difflib.
    #[test]
    fn ratio_matches_python() {
        let raw = include_str!("../ratio_vectors.json");
        let vs: Vec<RatioVec> = serde_json::from_str(raw).expect("vectors parse");
        assert!(vs.len() >= 30, "expected a real vector set, got {}", vs.len());
        let mut worst = 0.0f64;
        for v in &vs {
            let got = ratio(&v.a, &v.b);
            let d = (got - v.ratio).abs();
            if d > worst {
                worst = d;
            }
            assert!(
                d < 1e-12,
                "ratio({:?}, {:?}) = {} but Python says {}",
                v.a, v.b, got, v.ratio
            );
        }
        eprintln!("{} vectors, worst delta {:.3e}", vs.len(), worst);
    }

    #[derive(Deserialize)]
    struct TextVec {
        s: String,
        tidy: String,
        norm_key: String,
        strip_feat: String,
        strip_noise: String,
        strip_all: String,
        primary_artist: String,
    }

    #[test]
    fn normalisers_match_python() {
        let raw = include_str!("../text_vectors.json");
        let vs: Vec<TextVec> = serde_json::from_str(raw).expect("vectors parse");
        let mut bad = Vec::new();
        for v in &vs {
            let checks: [(&str, String, &String); 6] = [
                ("tidy", tidy(&v.s), &v.tidy),
                ("norm_key", norm_key(&v.s), &v.norm_key),
                ("strip_feat", strip_feat(&v.s), &v.strip_feat),
                ("strip_noise", strip_noise_brackets(&v.s), &v.strip_noise),
                ("strip_all", strip_all_brackets(&v.s), &v.strip_all),
                ("primary_artist", primary_artist(&v.s), &v.primary_artist),
            ];
            for (name, got, want) in checks {
                if &got != want {
                    bad.push(format!("{}({:?}): got {:?} want {:?}", name, v.s, got, want));
                }
            }
        }
        assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
    }
}
