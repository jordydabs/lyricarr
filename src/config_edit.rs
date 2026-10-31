//! Changing one value in `config.toml` without destroying the file.
//!
//! The obvious approach - parse to a struct, serialise back - would throw away
//! every comment in that file, and those comments are the documentation for
//! what each knob does and why it is set the way it is. So the Settings tab
//! rewrites exactly the one line that holds the value, inside the right
//! `[section]`, and leaves the rest of the bytes untouched.

use anyhow::{bail, Result};
use std::path::Path;

/// Replace `key = <value>` inside `[section]`. Returns false if the key was not
/// found, so the caller can tell "changed nothing" from "worked".
pub fn set_value(path: &Path, section: &str, key: &str, value: &str) -> Result<bool> {
    if !path.is_file() {
        bail!("{} does not exist", path.display());
    }
    let text = std::fs::read_to_string(path)?;
    // Keep the file's own line endings rather than imposing ours.
    let crlf = text.contains("\r\n");
    let mut out: Vec<String> = Vec::new();
    let mut in_section = section.is_empty();
    let mut done = false;

    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with('[') {
            let name = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            in_section = name.eq_ignore_ascii_case(section);
            out.push(raw.to_string());
            continue;
        }
        if in_section && !done {
            // `key` at the start of the line (allowing indentation), then `=`
            let is_comment = trimmed.starts_with('#');
            if !is_comment {
                if let Some(eq) = trimmed.find('=') {
                    if trimmed[..eq].trim() == key {
                        let indent: String = raw
                            .chars()
                            .take_while(|c| c.is_whitespace())
                            .collect();
                        // preserve a trailing comment on the same line
                        let after = &trimmed[eq + 1..];
                        let comment = find_trailing_comment(after);
                        out.push(format!(
                            "{indent}{key} = {value}{comment}",
                            indent = indent,
                            key = key,
                            value = value,
                            comment = comment,
                        ));
                        done = true;
                        continue;
                    }
                }
            }
        }
        out.push(raw.to_string());
    }

    if !done {
        return Ok(false);
    }
    let sep = if crlf { "\r\n" } else { "\n" };
    let mut body = out.join(sep);
    body.push_str(sep);
    // Write via a temp file so a crash cannot leave a half-written config.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Render a list of paths as a TOML array of *literal* strings.
///
/// Literal strings (single quotes) take backslashes verbatim, which is the
/// whole reason config.toml uses them for Windows paths. A literal string
/// cannot hold a single quote or a control character - and a Linux folder
/// name can contain a newline - so those fall back to a basic string with
/// everything escaped. Rare, but silently writing a broken config is not an
/// option.
pub fn toml_path_array(paths: &[String]) -> String {
    let parts: Vec<String> = paths
        .iter()
        .map(|p| {
            if p.contains('\'') || p.chars().any(|c| c.is_control()) {
                toml_basic_string(p)
            } else {
                format!("'{}'", p)
            }
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

/// A TOML basic (double-quoted) string, escaped per the TOML spec.
fn toml_basic_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod array_tests {
    use super::toml_path_array;

    #[test]
    fn windows_paths_stay_literal() {
        let v = vec![r"D:\Music".to_string(), r"E:\More Music".to_string()];
        assert_eq!(toml_path_array(&v), r"['D:\Music', 'E:\More Music']");
    }

    #[test]
    fn a_quote_forces_a_basic_string_with_escapes() {
        let v = vec![r"D:\Bob's Music".to_string()];
        // literal strings cannot hold a single quote, so this must switch form
        let out = toml_path_array(&v);
        assert!(out.starts_with("[\""), "{out}");
        assert!(out.contains(r"D:\\Bob's Music"), "{out}");
    }

    #[test]
    fn empty_is_an_empty_array() {
        assert_eq!(toml_path_array(&[]), "[]");
    }

    /// Whatever the folder is called, the line written must parse back to
    /// exactly that folder.
    #[test]
    fn any_path_survives_a_round_trip() {
        let odd = vec![
            r"D:\Music".to_string(),
            r"D:\Bob's Music".to_string(),
            "/srv/music/with\nnewline".to_string(),
            "/srv/tab\there/\"quoted\"".to_string(),
            "/srv/bell\u{7}/del\u{7f}".to_string(),
            "/srv/Björk/ünïcode".to_string(),
        ];
        let line = format!("roots = {}", toml_path_array(&odd));
        #[derive(serde::Deserialize)]
        struct R {
            roots: Vec<String>,
        }
        let back: R = toml::from_str(&line).unwrap_or_else(|e| panic!("{line}\n{e}"));
        assert_eq!(back.roots, odd);
    }
}

/// A `#` that is outside a quoted string starts a trailing comment; one inside
/// quotes (`plain_lyrics = "lrc"  # like this`) does not.
fn find_trailing_comment(after_eq: &str) -> String {
    let mut q: Option<char> = None;
    for (i, c) in after_eq.char_indices() {
        match q {
            Some(open) => {
                if c == open {
                    q = None;
                }
            }
            None => match c {
                '"' | '\'' => q = Some(c),
                '#' => {
                    let mut s = String::from("  ");
                    s.push_str(after_eq[i..].trim_end());
                    return s;
                }
                _ => {}
            },
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own file. They share a process and cargo runs them in
    /// parallel, so one path for all of them meant three tests editing the same
    /// config and reading each other's results - which is exactly how this
    /// first failed.
    fn roundtrip(name: &str, body: &str, section: &str, key: &str, val: &str) -> String {
        let dir = std::env::temp_dir().join(format!("lyr-cfg-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, body).unwrap();
        set_value(&p, section, key, val).unwrap();
        std::fs::read_to_string(&p).unwrap()
    }

    #[test]
    fn keeps_comments_and_other_sections() {
        let src = "# top comment\n\
                   [library]\n\
                   roots = ['D:\\\\Music']\n\n\
                   [lrclib]\n\
                   # politeness vs speed\n\
                   requests_per_second = 3.0\n\
                   workers = 4\n";
        let out = roundtrip("comments", src, "lrclib", "requests_per_second", "2.0");
        assert!(out.contains("# top comment"), "lost the header comment:\n{out}");
        assert!(out.contains("# politeness vs speed"), "lost the key's comment:\n{out}");
        assert!(out.contains("requests_per_second = 2.0"), "did not set:\n{out}");
        assert!(out.contains("workers = 4"), "clobbered a sibling:\n{out}");
        assert!(out.contains(r"roots = ['D:\\Music']"), "touched another section:\n{out}");
    }

    #[test]
    fn only_edits_the_named_section() {
        // `workers` in two sections: only the asked-for one may change
        let src = "[a]\nworkers = 1\n\n[lrclib]\nworkers = 4\n";
        let out = roundtrip("section", src, "lrclib", "workers", "8");
        assert!(out.contains("[a]\nworkers = 1"), "edited the wrong section:\n{out}");
        assert!(out.contains("[lrclib]\nworkers = 8"), "did not edit the right one:\n{out}");
    }

    #[test]
    fn preserves_a_trailing_comment() {
        let src = "[output]\noverwrite = false  # careful\n";
        let out = roundtrip("trailing", src, "output", "overwrite", "true");
        assert!(out.contains("overwrite = true  # careful"), "{out}");
    }

    #[test]
    fn missing_key_reports_false() {
        let dir = std::env::temp_dir().join(format!("lyr-cfg-{}-missing", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        std::fs::write(&p, "[lrclib]\nworkers = 4\n").unwrap();
        assert!(!set_value(&p, "lrclib", "nope", "1").unwrap());
    }
}
