//! Soloist's output: read for what it tells (when the build expires, which
//! file format a track plays from), and passed on to the daemon's log only
//! as warnings and errors, with anything secret-looking taken out.
//!
//! Soloist runs with `--verbose`: only its decoder log says what it plays.
//! A track's start reads
//!
//! ```text
//! 22:51:29.492 I [f:1699 ] AudioTrackPlayer: playing current track. Start Reason: "clickrow", Track URI: spotify:track:…
//! 22:51:29.763 I [f:346  ] Opening sink for playback.
//! 22:51:29.763 I [f:347  ]     codec: vorbis
//! 22:51:29.763 I [f:348  ]     bitrate: 320000 bits/s
//! 22:51:29.764 I [f:349  ]     sample rate: 44100 Hz
//! ```
//!
//! and the sink's lines name the format of the track started last. (Its
//! `sample format: 32-bit-float-pcm` is the decoder's output, not the file's
//! bit depth.)

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

/// The format a track is played from, as Soloist's decoder opened it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Format {
    /// The track it was opened for.
    #[serde(skip)]
    pub uri: String,
    /// `flac` or `vorbis` (what Soloist names it).
    pub codec: String,
    /// The bitrate the decoder declares (FLAC's is nominal; the file's own
    /// average varies by track).
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
}

/// What a line said that the player keeps.
#[derive(Debug, PartialEq, Eq)]
pub enum Note {
    /// "client expires in N days".
    ExpiresInDays(u32),
    /// The build is past its date; Soloist exits with code 10.
    Expired,
    /// The sink opened for a track: its format is complete.
    Format(Format),
}

/// Follows the output line by line.
#[derive(Default)]
pub struct Reader {
    /// The track started last.
    track: Option<String>,
    /// A sink being described, until its last line.
    sink: Option<Format>,
}

static EXPIRES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"client expires in (\d+) day").expect("regex"));
static TRACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"playing current track\..*Track URI: (spotify:[a-z]+:[0-9A-Za-z]+)").expect("regex"));
static VERBOSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d\d:\d\d:\d\d\.\d+ ([A-Z]) \[[^\]]*\] ?(.*)$").expect("regex"));
/// What a line without --verbose's prefix starts with.
static PLAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d+: ").expect("regex"));
/// Terminal colours.
static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("regex"));
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\b(?:https?|wss?)://[^\s"'<>]+"#).expect("regex"));
static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(api[_-]?key|token|access_token|refresh_token|code|blob|secret|password|authorization|bearer)(["']?\s*[:=]\s*["']?|\s+)[^\s"',;&]+"#)
        .expect("regex")
});
/// Long opaque strings (keys, tokens, signed paths) wherever they are.
static OPAQUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9_\-+/=.]{40,}").expect("regex"));

/// `line` with URLs, `name=value` secrets, long opaque strings and `api_key`
/// itself replaced.
pub fn redact(line: &str, api_key: &str) -> String {
    let mut s = if api_key.len() >= 8 { line.replace(api_key, "<api-key>") } else { line.to_owned() };
    s = URL
        .replace_all(&s, |c: &regex::Captures| {
            let url = &c[0];
            let host = url.split("://").nth(1).and_then(|r| r.split(['/', '?', '#']).next()).unwrap_or("");
            format!("<url {host}>")
        })
        .into_owned();
    s = SECRET.replace_all(&s, "$1$2<redacted>").into_owned();
    OPAQUE.replace_all(&s, "<redacted>").into_owned()
}

impl Reader {
    /// Reads one line: what it says, and whether (redacted) the daemon's log
    /// should show it, and at which level.
    pub fn line(&mut self, line: &str, api_key: &str) -> (Option<Note>, Option<(log::Level, String)>) {
        let line = ANSI.replace_all(line, "");
        let (level, text) = match VERBOSE.captures(&line) {
            Some(c) => (c.get(1).map_or("I", |m| m.as_str()), c.get(2).map_or("", |m| m.as_str())),
            // Unprefixed lines are what Soloist prints without --verbose.
            None => ("N", PLAIN.find(&line).map_or(&*line, |m| &line[m.end()..])),
        };
        let mut note = None;
        if let Some(c) = EXPIRES.captures(text) {
            note = c[1].parse().ok().map(Note::ExpiresInDays);
        } else if text.contains("client expired") {
            note = Some(Note::Expired);
        } else if let Some(c) = TRACK.captures(text) {
            self.track = Some(c[1].to_owned());
            self.sink = None;
        } else if text.starts_with("Opening sink for playback") {
            self.sink = self.track.clone().map(|uri| Format { uri, codec: String::new(), bitrate_kbps: None, sample_rate_hz: None });
        } else if let Some(sink) = self.sink.as_mut() {
            let field = text.trim();
            if let Some(codec) = field.strip_prefix("codec: ") {
                sink.codec = codec.trim().to_owned();
            } else if let Some(rate) = field.strip_prefix("bitrate: ") {
                sink.bitrate_kbps = rate.split_whitespace().next().and_then(|n| n.parse::<u32>().ok()).map(|b| b / 1000);
            } else if let Some(rate) = field.strip_prefix("sample rate: ") {
                sink.sample_rate_hz = rate.split_whitespace().next().and_then(|n| n.parse().ok());
                // The last line the format needs.
                if !sink.codec.is_empty() {
                    note = self.sink.take().map(Note::Format);
                }
            }
        }
        let level = match level {
            "E" | "F" => Some(log::Level::Error),
            "W" => Some(log::Level::Warn),
            "N" => Some(log::Level::Info),
            // What it plays and pauses, one line each: the daemon log's story.
            _ if text.starts_with("playing spotify:") || text.starts_with("paused spotify:") => Some(log::Level::Info),
            _ => None,
        };
        let shown = level.filter(|_| !noise(text)).map(|level| (level, redact(text, api_key)));
        (note, shown)
    }
}

/// Lines Soloist prints all the time that tell nothing (on its first start
/// it has no settings file yet).
fn noise(text: &str) -> bool {
    text.contains("not found in cache") || text.contains("Unable to open settings file") || text.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: &[&str] = &[
        r#"22:51:29.492 I [f:1699    ] AudioTrackPlayer: playing current track. Start Reason: "clickrow", Track URI: spotify:track:4Dd0onhwJQortMphWLaKzh"#,
        "22:51:29.616 I [f:134     ] FileStreamer file average bitrate: 320000",
        "22:51:29.763 I [f:346     ] Opening sink for playback.",
        "22:51:29.763 I [f:347     ]     codec: vorbis",
        "22:51:29.763 I [f:348     ]     bitrate: 320000 bits/s",
        "22:51:29.764 I [f:349     ]     sample rate: 44100 Hz",
        "22:51:29.764 I [f:350     ]     sample format: 32-bit-float-pcm",
    ];

    #[test]
    fn the_sink_names_the_format_of_the_track_started_last() {
        let mut r = Reader::default();
        let notes: Vec<Note> = START.iter().filter_map(|l| r.line(l, "").0).collect();
        assert_eq!(
            notes,
            vec![Note::Format(Format {
                uri: "spotify:track:4Dd0onhwJQortMphWLaKzh".to_owned(),
                codec: "vorbis".to_owned(),
                bitrate_kbps: Some(320),
                sample_rate_hz: Some(44100),
            })]
        );
        // A sink opened before any track started (restored prefetch) names none.
        let mut fresh = Reader::default();
        assert!(START[2..].iter().all(|l| fresh.line(l, "").0.is_none()));
    }

    #[test]
    fn expiry_and_levels() {
        let mut r = Reader::default();
        let (note, shown) = r.line("2026-10-21 15:52:19.853: client expires in 89 days", "");
        assert_eq!(note, Some(Note::ExpiresInDays(89)));
        assert_eq!(shown.map(|s| s.0), Some(log::Level::Info));
        assert_eq!(r.line("client expired, please update to a newer version. Exiting with exit code 10", "").0, Some(Note::Expired));
        assert!(r.line("22:51:29.492 I [f:1 ] Playing music set to 'true'", "").1.is_none());
        assert_eq!(r.line("22:51:29.492 W [f:1 ] something odd", "").1.map(|s| s.0), Some(log::Level::Warn));
    }

    #[test]
    fn redact_hides_keys_tokens_and_urls() {
        let key = "0123456789abcdef0123";
        let line = format!(
            "W api key {key} rejected; GET https://audio-fa.scdn.co/audio/abc?token=xyz failed; access_token=eyJhbGciOi bearer abc.def"
        );
        let out = redact(&line, key);
        assert!(!out.contains(key) && !out.contains("xyz") && !out.contains("eyJhbGciOi") && !out.contains("abc.def"), "{out}");
        assert!(out.contains("<url audio-fa.scdn.co>"), "{out}");
    }
}
