//! Reading what a file says about itself.
//!
//! Filenames are never parsed. The library's names are mangled
//! (`Artist - Album -  Disc  - 4 - Title.mp3`, empty disc fields,
//! `;`-joined collaborators) and a wrong `.lrc` is worse than none, so every
//! search term comes from tags - which Lidarr writes cleanly.

use crate::text::tidy;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Track {
    pub path: String,
    pub mtime: f64,
    pub size: i64,
    pub title: String,
    pub artist: String,
    pub albumartist: String,
    pub album: String,
    pub duration: f64,
}

impl Track {
    /// What to search with: the track artist, falling back to the album artist.
    pub fn search_artist(&self) -> &str {
        if !self.artist.is_empty() { &self.artist } else { &self.albumartist }
    }
    /// A file with no title or no artist cannot be looked up at all.
    pub fn usable(&self) -> bool {
        !self.title.is_empty() && !self.search_artist().is_empty()
    }
}

/// Read tags and duration. Never fails: an unreadable or tagless file comes
/// back with empty strings and is recorded as `notags`, which is a real
/// outcome rather than an error to retry.
pub fn read_track(path: &Path, mtime: f64, size: i64) -> Track {
    let mut tr = Track {
        path: path.to_string_lossy().to_string(),
        mtime,
        size,
        ..Default::default()
    };

    let tagged = match Probe::open(path).and_then(|p| p.read()) {
        Ok(t) => t,
        Err(_) => return tr,
    };

    tr.duration = tagged.properties().duration().as_secs_f64();

    // primary_tag is the format's preferred tag (ID3v2 on mp3, Vorbis on
    // flac); first_tag covers files that only carry a secondary one.
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    if let Some(tag) = tag {
        let get = |k: &ItemKey| tidy(tag.get_string(k).unwrap_or(""));
        tr.title = get(&ItemKey::TrackTitle);
        tr.artist = get(&ItemKey::TrackArtist);
        tr.albumartist = get(&ItemKey::AlbumArtist);
        tr.album = get(&ItemKey::AlbumTitle);
    }
    tr
}

/// True if the file already carries lyrics in its tags. Presence only - the
/// text is never read into memory beyond a length check, same as the Python.
pub fn has_embedded_lyrics(path: &Path) -> bool {
    let Ok(tagged) = Probe::open(path).and_then(|p| p.read()) else {
        return false;
    };
    for tag in tagged.tags() {
        if let Some(v) = tag.get_string(&ItemKey::Lyrics) {
            if v.len() > 20 {
                return true;
            }
        }
        // Some writers park lyrics in an unmapped frame (USLT/SYLT variants,
        // ©lyr on m4a). Checking the raw keys too mirrors the Python, which
        // scanned every tag key for "LYRIC".
        for item in tag.items() {
            let key = format!("{:?}", item.key()).to_uppercase();
            if key.contains("LYRIC") || key.contains("USLT") || key.contains("SYLT") {
                if let Some(s) = item.value().text() {
                    if s.len() > 20 {
                        return true;
                    }
                }
            }
        }
    }
    false
}
