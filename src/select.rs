//! yt-dlp's format selection (`-f`) on an info dict yt-dlp has already resolved, so an app
//! that shows a preview first doesn't have to start Python a second time just to choose.
//!
//! It follows yt-dlp's rules for what apps ask for: `/` fallbacks, `+` merges, `b`/`w` with
//! `v`/`a`/`*` and `.N`, format ids, extension shortcuts and `[...]` filters (`<`, `<=`, `>`,
//! `>=`, `=`, `!=` on numbers; `=`, `^=`, `$=`, `*=` and their `!` negation on strings; `?` lets
//! a missing value pass). Groups, `,`, `all` and `~=` regexes are [`Unsupported`]: the caller
//! should let yt-dlp choose instead.

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// A selector cheek can't evaluate itself; yt-dlp can.
#[derive(Debug)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "format selector {:?} isn't supported yet; use yt-dlp to choose formats", self.0)
    }
}

impl std::error::Error for Unsupported {}

/// Applies `spec` to `info` and returns the info dict the way `yt-dlp -J -f spec` prints it:
/// `requested_formats` for a merge, the chosen format's fields at the top level otherwise.
pub fn apply(info: &Value, spec: &str) -> Result<Value> {
    if !info.is_object() {
        bail!("the info JSON isn't an object");
    }
    let selector = parse(spec)?;
    let formats: Vec<&Value> = match info.get("formats").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().collect(),
        _ => vec![info],
    };
    let ctx = Ctx {
        has_merged: formats.iter().any(|f| !none(f, "vcodec") && !none(f, "acodec")),
        incomplete: formats.iter().all(|f| none(f, "vcodec")) || formats.iter().all(|f| none(f, "acodec")),
        formats,
    };
    let picked = selector
        .iter()
        .find_map(|merge| pick(merge, &ctx))
        .ok_or_else(|| anyhow!("Requested format is not available"))?;
    Ok(build(info, &picked))
}

struct Atom {
    name: String,
    filters: Vec<Filter>,
}

enum Filter {
    Num {
        key: String,
        op: &'static str,
        value: f64,
        none_ok: bool,
    },
    Str {
        key: String,
        op: &'static str,
        negate: bool,
        value: String,
        none_ok: bool,
    },
}

struct Ctx<'a> {
    formats: Vec<&'a Value>,
    /// Some format carries both picture and sound.
    has_merged: bool,
    /// The site offers only video-only or only audio-only formats.
    incomplete: bool,
}

fn none(f: &Value, codec: &str) -> bool {
    f.get(codec).and_then(Value::as_str) == Some("none")
}

/// `a/b+c` becomes `[[a], [b, c]]`.
fn parse(spec: &str) -> Result<Vec<Vec<Atom>>> {
    let unsupported = || anyhow!(Unsupported(spec.to_string()));
    let mut alternatives: Vec<Vec<Atom>> = vec![Vec::new()];
    let mut name = String::new();
    let mut filters = Vec::new();
    let mut chars = spec.chars().peekable();
    loop {
        let c = chars.next();
        match c {
            None | Some('/') | Some('+') => {
                if name.is_empty() && filters.is_empty() {
                    return Err(unsupported());
                }
                if name.is_empty() {
                    name.push_str("best");
                }
                alternatives.last_mut().unwrap().push(Atom {
                    name: std::mem::take(&mut name),
                    filters: std::mem::take(&mut filters),
                });
                match c {
                    None => break,
                    Some('/') => alternatives.push(Vec::new()),
                    _ => {}
                }
            }
            Some('[') => {
                let mut body = String::new();
                let mut quote = None;
                loop {
                    let Some(ch) = chars.next() else { return Err(unsupported()) };
                    match (quote, ch) {
                        (None, ']') => break,
                        (None, '"' | '\'') => quote = Some(ch),
                        (Some(q), _) if ch == q => quote = None,
                        (Some(_), '\\') => {
                            body.push(ch);
                            if let Some(next) = chars.next() {
                                body.push(next);
                            }
                            continue;
                        }
                        _ => {}
                    }
                    body.push(ch);
                }
                filters.push(filter(&body).ok_or_else(unsupported)?);
            }
            Some(c) if c.is_whitespace() => {}
            Some('(' | ')' | ',' | ']') => return Err(unsupported()),
            Some(c) => {
                if !filters.is_empty() {
                    return Err(unsupported());
                }
                name.push(c);
            }
        }
    }
    for atom in alternatives.iter().flatten() {
        if matches!(atom.name.as_str(), "all" | "mergeall" | "mhtml") {
            return Err(unsupported());
        }
    }
    Ok(alternatives)
}

static NUM_FILTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([\w.-]+)\s*(<=|>=|!=|<|>|=)(\s*\?)?\s*([0-9.]+(?:[kKmMgGtTpPeEzZyY]i?[Bb]?)?)\s*$").unwrap());
static STR_FILTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*([a-zA-Z0-9._-]+)\s*(!\s*)?(\^=|\$=|\*=|~=|=)\s*(\?\s*)?(?:"((?:[^"\\]|\\.)+)"|'((?:[^'\\]|\\.)+)'|([\w.-]+))\s*$"#)
        .unwrap()
});
static UNESCAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\\([\\"'])"#).unwrap());

/// One `[...]` condition, read the way yt-dlp reads it: numeric first, then string.
fn filter(body: &str) -> Option<Filter> {
    if let Some(m) = NUM_FILTER.captures(body) {
        let raw = &m[4];
        let value = raw
            .parse::<i64>()
            .ok()
            .map(|n| n as f64)
            .or_else(|| parse_filesize(raw))
            .or_else(|| parse_filesize(&format!("{raw}B")))?;
        let op = match &m[2] {
            "<=" => "<=",
            ">=" => ">=",
            "!=" => "!=",
            "<" => "<",
            ">" => ">",
            _ => "=",
        };
        return Some(Filter::Num {
            key: m[1].to_string(),
            op,
            value,
            none_ok: m.get(3).is_some(),
        });
    }
    let m = STR_FILTER.captures(body)?;
    let op = match &m[3] {
        "^=" => "^=",
        "$=" => "$=",
        "*=" => "*=",
        "=" => "=",
        _ => return None, // ~= is a Python regex
    };
    let value = m.get(5).or(m.get(6)).map(|v| UNESCAPE.replace_all(v.as_str(), "$1").into_owned());
    Some(Filter::Str {
        key: m[1].to_string(),
        op,
        negate: m.get(2).is_some(),
        value: value.unwrap_or_else(|| m[7].to_string()),
        none_ok: m.get(4).is_some(),
    })
}

/// yt-dlp's `parse_filesize`: `M`/`MB` are powers of 1000, `Mi`/`MiB` and lowercase `kB` of 1024.
fn parse_filesize(s: &str) -> Option<f64> {
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))?;
    let (num, unit) = s.split_at(split);
    let num: f64 = num.replace(',', ".").parse().ok()?;
    let unit = unit.trim();
    let mult = match unit {
        "B" | "b" | "bytes" => 1.0,
        _ => {
            let mut chars = unit.chars();
            let prefix = chars.next()?;
            let power = "KMGTPEZY".find(prefix.to_ascii_uppercase())? as i32 + 1;
            match chars.as_str() {
                "iB" if prefix.is_ascii_uppercase() => 1024f64.powi(power),
                "B" if prefix.is_ascii_uppercase() => 1000f64.powi(power),
                "B" => 1024f64.powi(power),
                "b" => 1000f64.powi(power),
                _ => return None,
            }
        }
    };
    Some((num * mult).trunc())
}

impl Filter {
    fn matches(&self, f: &Value) -> bool {
        match self {
            Filter::Num { key, op, value, none_ok } => {
                let actual = match f.get(key) {
                    None | Some(Value::Null) => return *none_ok,
                    Some(Value::Number(n)) => n.as_f64(),
                    Some(Value::String(s)) => s.parse::<f64>().ok(),
                    Some(_) => None,
                };
                let Some(a) = actual else { return false };
                match *op {
                    "<" => a < *value,
                    "<=" => a <= *value,
                    ">" => a > *value,
                    ">=" => a >= *value,
                    "!=" => a != *value,
                    _ => a == *value,
                }
            }
            Filter::Str {
                key,
                op,
                negate,
                value,
                none_ok,
            } => {
                let actual = match f.get(key) {
                    None | Some(Value::Null) => return *none_ok,
                    Some(v) => v.as_str(),
                };
                let hit = actual.is_some_and(|a| match *op {
                    "^=" => a.starts_with(value.as_str()),
                    "$=" => a.ends_with(value.as_str()),
                    "*=" => a.contains(value.as_str()),
                    _ => a == value,
                });
                hit != *negate
            }
        }
    }
}

static BEST_WORST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(best|worst|b|w)(video|audio|v|a)?(\*)?(?:\.([1-9]\d*))?$").unwrap());

/// One format for one atom, like yt-dlp's `_build_selector_function` for a single name.
fn select<'a>(atom: &Atom, ctx: &Ctx<'a>) -> Option<&'a Value> {
    let pool: Vec<&'a Value> = ctx
        .formats
        .iter()
        .copied()
        .filter(|f| atom.filters.iter().all(|flt| flt.matches(f)))
        .collect();
    let any_codec = |f: &Value| !none(f, "vcodec") || !none(f, "acodec");
    let ext_is = |f: &Value, ext: &str| f.get("ext").and_then(Value::as_str) == Some(ext);
    let name = atom.name.as_str();

    let (mut matches, best, index): (Vec<&'a Value>, bool, usize) = if let Some(m) = BEST_WORST.captures(name) {
        let best = m[1].starts_with('b');
        let index = m.get(4).and_then(|n| n.as_str().parse().ok()).unwrap_or(1);
        let kind = m.get(2).map(|t| &t.as_str()[..1]);
        let modified = m.get(3).is_some();
        let wanted = |f: &Value| {
            any_codec(f)
                && match (kind, modified) {
                    (Some("v"), true) => !none(f, "vcodec"),
                    (Some("a"), true) => !none(f, "acodec"),
                    (Some("v"), false) => none(f, "acodec"),
                    (Some(_), false) => none(f, "vcodec"),
                    (None, false) => !none(f, "vcodec") && !none(f, "acodec"),
                    (_, true) => true,
                }
        };
        let mut matches: Vec<_> = pool.iter().copied().filter(|f| wanted(f)).collect();
        // Sites with only video-only or only audio-only formats: `b` takes the best of those.
        if matches.is_empty() && kind.is_none() && !modified && ctx.incomplete {
            matches = pool.iter().copied().filter(|f| any_codec(f)).collect();
        }
        (matches, best, index)
    } else if matches!(name, "m4a" | "mp3" | "ogg" | "aac") {
        (
            pool.iter().copied().filter(|f| ext_is(f, name) && !none(f, "acodec")).collect(),
            true,
            1,
        )
    } else if matches!(name, "mp4" | "flv" | "webm" | "3gp") {
        let mut matches: Vec<_> = pool
            .iter()
            .copied()
            .filter(|f| ext_is(f, name) && !none(f, "acodec") && !none(f, "vcodec"))
            .collect();
        if matches.is_empty() && !ctx.has_merged {
            matches = pool.iter().copied().filter(|f| ext_is(f, name) && !none(f, "vcodec")).collect();
        }
        (matches, true, 1)
    } else {
        let id = |f: &Value| match f.get("format_id") {
            Some(Value::String(s)) => s == name,
            Some(Value::Number(n)) => n.to_string() == name,
            _ => false,
        };
        (pool.iter().copied().filter(|f| id(f)).collect(), true, 1)
    };
    // yt-dlp sorts formats worst to best.
    if best {
        matches.reverse();
    }
    matches.get(index - 1).copied()
}

/// Every atom of a `+` merge, minus what would bring a second picture or sound, as yt-dlp
/// does without `--video-multistreams`/`--audio-multistreams`.
fn pick<'a>(merge: &[Atom], ctx: &Ctx<'a>) -> Option<Vec<&'a Value>> {
    let chosen = merge.iter().map(|atom| select(atom, ctx)).collect::<Option<Vec<_>>>()?;
    let (mut video, mut audio) = (false, false);
    let kept: Vec<&Value> = chosen
        .into_iter()
        .filter(|f| {
            let (v, a) = (!none(f, "vcodec"), !none(f, "acodec"));
            if !v && !a || v && video || a && audio {
                return false;
            }
            video |= v;
            audio |= a;
            true
        })
        .collect();
    (!kept.is_empty()).then_some(kept)
}

/// Fields that describe one format; the old selection's go before the new one's come in.
const FORMAT_KEYS: &[&str] = &[
    "url",
    "manifest_url",
    "fragment_base_url",
    "fragments",
    "protocol",
    "ext",
    "format",
    "format_id",
    "format_note",
    "vcodec",
    "acodec",
    "width",
    "height",
    "resolution",
    "fps",
    "tbr",
    "vbr",
    "abr",
    "asr",
    "filesize",
    "filesize_approx",
    "downloader_options",
    "has_drm",
];

fn build(info: &Value, picked: &[&Value]) -> Value {
    let mut out = info.clone();
    let obj = out.as_object_mut().expect("checked in apply");
    obj.remove("requested_formats");
    if let [one] = picked {
        if std::ptr::eq(*one, info) {
            return out;
        }
        for key in FORMAT_KEYS {
            obj.remove(*key);
        }
        if let Some(fields) = one.as_object() {
            obj.extend(fields.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    } else {
        for key in FORMAT_KEYS {
            obj.remove(*key);
        }
        let ids: Vec<&str> = picked.iter().filter_map(|f| f.get("format_id").and_then(Value::as_str)).collect();
        obj.insert("format_id".into(), Value::String(ids.join("+")));
        obj.insert(
            "requested_formats".into(),
            Value::Array(picked.iter().map(|f| (*f).clone()).collect()),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(id: &str, ext: &str, vcodec: &str, acodec: &str, height: Option<u32>) -> Value {
        json!({"format_id": id, "url": format!("https://v/{id}"), "ext": ext, "vcodec": vcodec, "acodec": acodec,
               "height": height, "protocol": "https"})
    }

    /// Like YouTube, worst first.
    fn youtube() -> Value {
        json!({
            "id": "x", "title": "t", "extractor_key": "Youtube", "url": "https://v/old", "format_id": "18",
            "requested_formats": [{"format_id": "old"}],
            "formats": [
                {"format_id": "sb0", "ext": "mhtml", "vcodec": "none", "acodec": "none", "protocol": "mhtml", "url": "https://v/sb0"},
                f("139", "m4a", "none", "mp4a.40.5", None),
                f("249", "webm", "none", "opus", None),
                f("140", "m4a", "none", "mp4a.40.2", None),
                f("251", "webm", "none", "opus", None),
                f("18", "mp4", "avc1.42001E", "mp4a.40.2", Some(360)),
                f("134", "mp4", "avc1.4d401e", "none", Some(360)),
                f("136", "mp4", "avc1.4d401f", "none", Some(720)),
                f("247", "webm", "vp9", "none", Some(720)),
                f("137", "mp4", "avc1.640028", "none", Some(1080)),
                f("248", "webm", "vp9", "none", Some(1080)),
            ]
        })
    }

    fn ids(v: &Value) -> String {
        v["format_id"].as_str().unwrap().to_string()
    }

    fn pick_ids(info: &Value, spec: &str) -> String {
        ids(&apply(info, spec).unwrap())
    }

    #[test]
    fn hamstr_selectors() {
        let v = youtube();
        assert_eq!(pick_ids(&v, "bv*+ba/b"), "248+251");
        assert_eq!(pick_ids(&v, "bv*[height<=720]+ba/b[height<=720]/bv*+ba/b"), "247+251");
        assert_eq!(pick_ids(&v, "bv*[height<=240]+ba/b[height<=240]/bv*+ba/b"), "248+251");
        assert_eq!(pick_ids(&v, "ba[ext=m4a]/ba[acodec^=mp4a]/ba[acodec=aac]/ba[ext=mp4]/ba/b"), "140");
        assert_eq!(pick_ids(&v, "ba[acodec^=opus]/ba[ext=opus]/ba[ext=ogg]/ba/b"), "251");
        assert_eq!(pick_ids(&v, "ba[ext=mp3]/ba/b"), "251");
    }

    #[test]
    fn best_worst_and_shortcuts() {
        let v = youtube();
        assert_eq!(pick_ids(&v, "b"), "18");
        assert_eq!(pick_ids(&v, "w"), "18");
        assert_eq!(pick_ids(&v, "bv"), "248");
        assert_eq!(pick_ids(&v, "wv"), "134");
        assert_eq!(pick_ids(&v, "bv.2"), "137");
        assert_eq!(pick_ids(&v, "worstaudio"), "139");
        assert_eq!(pick_ids(&v, "mp4"), "18");
        assert_eq!(pick_ids(&v, "m4a"), "140");
        assert_eq!(pick_ids(&v, "137+140"), "137+140");
        assert_eq!(pick_ids(&v, "bv*[height=720]"), "247");
        // A bare filter means `b`, which needs picture and sound in one format.
        assert_eq!(pick_ids(&v, "[height=360]"), "18");
        assert!(apply(&v, "[height=720]").is_err());
        assert!(apply(&v, "b.2").is_err());
    }

    #[test]
    fn a_merge_drops_a_second_sound() {
        // 18 already has sound, so 140 isn't merged in.
        let out = apply(&youtube(), "18+140").unwrap();
        assert_eq!(ids(&out), "18");
        assert!(out.get("requested_formats").is_none());
        assert_eq!(out["url"], "https://v/18");
        assert_eq!(out["height"], 360);
    }

    #[test]
    fn output_reads_like_yt_dlp() {
        let merged = apply(&youtube(), "bv*[height<=720]+ba").unwrap();
        let list = merged["requested_formats"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert!(merged.get("url").is_none());
        let media = crate::extract::ytdlp::from_info_json(&merged).unwrap();
        assert_eq!(media.streams[0].height, Some(720));
        assert_eq!(media.streams[1].codec.as_deref(), Some("opus"));
    }

    #[test]
    fn filters() {
        let mut v = youtube();
        v["formats"][7]["filesize"] = json!(30_000_000);
        v["formats"][9]["filesize"] = json!(60_000_000);
        assert_eq!(pick_ids(&v, "bv[filesize<50M]"), "136");
        assert_eq!(pick_ids(&v, "bv[filesize<50Mi]"), "136");
        // A missing value fails a filter unless `?` lets it through.
        assert_eq!(pick_ids(&v, "bv[filesize>?50M]"), "248");
        assert_eq!(pick_ids(&v, "bv[vcodec!^=vp9]"), "137");
        assert_eq!(pick_ids(&v, "bv[vcodec*=\"640\"]"), "137");
        assert_eq!(pick_ids(&v, "ba[format_id=249]"), "249");
        assert_eq!(parse_filesize("500kB"), Some(512000.0));
        assert_eq!(parse_filesize("1.5GB"), Some(1.5e9));
    }

    #[test]
    fn audio_only_sites_fall_back() {
        let v = json!({"id": "s", "formats": [f("mp3-128", "mp3", "none", "mp3", None), f("opus-64", "opus", "none", "opus", None)]});
        assert_eq!(pick_ids(&v, "bv*+ba/b"), "opus-64");
        assert_eq!(pick_ids(&v, "b"), "opus-64");
    }

    #[test]
    fn a_single_format_info() {
        let v = json!({"id": "x", "url": "https://v/x.mp4", "ext": "mp4", "format_id": "0"});
        let out = apply(&v, "bv*+ba/b").unwrap();
        assert_eq!(out["url"], "https://v/x.mp4");
    }

    #[test]
    fn unsupported_selectors() {
        for spec in ["(bv/b)", "bv,ba", "bv[format_note~=HDR]", "all", "", "b/", "b[height<=720"] {
            let err = apply(&youtube(), spec).unwrap_err();
            assert!(err.downcast_ref::<Unsupported>().is_some(), "{spec}");
        }
    }
}
