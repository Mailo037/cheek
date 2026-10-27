//! Output names in yt-dlp's template syntax, so `-o` behaves the same in both tools.
//!
//! Supported: `%(field)s` and `%(field).NB` (cut to N bytes) for `title`, `id`, `ext`,
//! `extractor` and `uploader`.

use crate::media::Media;
use regex::Regex;
use std::sync::LazyLock;

// `%(f)s`, or `%(f).NB` where `B` is yt-dlp's "cut to N bytes" conversion (no trailing `s`).
static FIELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"%\((\w+)\)(?:\.(\d+)B|s)").unwrap());

pub fn render(template: &str, media: &Media, ext: &str) -> String {
    let out = FIELD.replace_all(template, |c: &regex::Captures| {
        let value = match &c[1] {
            "title" => media.title.clone(),
            "id" => media.id.clone(),
            "ext" => return ext.to_string(),
            "extractor" | "extractor_key" => media.extractor.clone(),
            "uploader" => media.uploader.clone().unwrap_or_else(|| "NA".into()),
            _ => "NA".into(),
        };
        let clean = sanitize(&value);
        match c.get(2).and_then(|n| n.as_str().parse::<usize>().ok()) {
            Some(limit) => truncate_bytes(&clean, limit).to_string(),
            None => clean,
        }
    });
    out.trim().to_string()
}

/// Makes a value safe for a file name the way yt-dlp does: characters that file systems
/// forbid become their full-width look-alikes, so titles stay readable.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '/' => '⧸',
            '\\' => '⧹',
            ':' => '：',
            '*' => '＊',
            '?' => '？',
            '"' => '＂',
            '<' => '＜',
            '>' => '＞',
            '|' => '｜',
            c => c,
        })
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .to_string()
}

fn truncate_bytes(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    let mut end = limit;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(title: &str) -> Media {
        Media {
            id: "abc123".into(),
            title: title.into(),
            extractor: "Generic".into(),
            webpage_url: "https://example.com".into(),
            thumbnail: None,
            duration: None,
            uploader: None,
            streams: vec![],
        }
    }

    #[test]
    fn renders_like_yt_dlp() {
        let m = media("Caminandes 3: Llamigos");
        assert_eq!(
            render("%(title)s [%(id)s].%(ext)s", &m, "mp4"),
            "Caminandes 3： Llamigos [abc123].mp4"
        );
    }

    #[test]
    fn cuts_on_char_boundaries() {
        let m = media("Ääää");
        assert_eq!(render("%(title).3B", &m, "mp4"), "Ä");
        assert_eq!(render("%(title).150B [%(id)s].%(ext)s", &m, "mp4"), "Ääää [abc123].mp4");
    }

    #[test]
    fn unknown_fields_become_na() {
        assert_eq!(render("%(nope)s.%(ext)s", &media("x"), "webm"), "NA.webm");
    }
}
