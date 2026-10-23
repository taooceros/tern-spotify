//! Audio quality: the one place that knows how Soloist is told which files to
//! stream, and where each quality's files are cached.
//!
//! Soloist has no quality command. It reads the bitrate tier from its
//! "classic" preferences at startup: `DATA/settings/prefs` (global; Soloist
//! may rewrite it without these keys) and
//! `DATA/settings/Users/<username>-user/prefs` (the account's, which keeps
//! them). Both are set before every start, other keys left as they are.
//! These are Soloist's internal preferences, not a published API.
//!
//! Soloist plays a file it has cached whatever the tier asks for: lowered
//! from Lossless to 320 kbit/s, a cached song still plays as FLAC. So each
//! quality gets its own cache directory, and they all share one size budget.

use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use serde::Serialize;

/// What the caches may hold together.
pub const CACHE_BUDGET_MB: u64 = 2048;
/// The smallest `--cache-size` Soloist takes.
const CACHE_MIN_MB: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// FLAC (Spotify's lossless files), where Spotify has them.
    Lossless,
    /// Ogg Vorbis at 320 kbit/s ("Very high").
    VeryHigh,
    /// Ogg Vorbis at 160 kbit/s ("High").
    High,
    /// Ogg Vorbis at 96 kbit/s ("Normal").
    Normal,
    /// AAC at 24 kbit/s ("Low"; Soloist logs the codec as `spac/aac`).
    Low,
}

impl Quality {
    pub const ALL: [Quality; 5] = [Quality::Lossless, Quality::VeryHigh, Quality::High, Quality::Normal, Quality::Low];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "lossless" => Some(Quality::Lossless),
            "very_high" | "320" => Some(Quality::VeryHigh),
            "high" | "160" => Some(Quality::High),
            "normal" | "96" => Some(Quality::Normal),
            "low" | "24" => Some(Quality::Low),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Quality::Lossless => "lossless",
            Quality::VeryHigh => "very_high",
            Quality::High => "high",
            Quality::Normal => "normal",
            Quality::Low => "low",
        }
    }

    /// Soloist's `audio.play_bitrate_enumeration` tier. Soloist takes 1–5
    /// and silently treats anything else as High.
    fn tier(self) -> u8 {
        match self {
            Quality::Lossless => 5,
            Quality::VeryHigh => 4,
            Quality::High => 3,
            Quality::Normal => 2,
            Quality::Low => 1,
        }
    }

    fn cache_name(self) -> &'static str {
        match self {
            Quality::Lossless => "lossless",
            Quality::VeryHigh => "320",
            Quality::High => "160",
            Quality::Normal => "96",
            Quality::Low => "24",
        }
    }
}

/// Where the daemon keeps Soloist: the files the user supplies, and what is
/// made from them.
#[derive(Clone, Debug)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    /// The official Soloist executable (downloaded by the user; a symlink is fine).
    pub fn binary(&self) -> PathBuf {
        self.root.join("soloist")
    }

    /// The user's own Soloist API key, alone in the file.
    pub fn api_key(&self) -> PathBuf {
        self.root.join("api-key")
    }

    /// Soloist's `--data-dir`: its signed-in session and settings.
    pub fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn quality_file(&self) -> PathBuf {
        self.root.join("quality")
    }

    /// The volume Soloist last reported (0–100), for its next start; Soloist
    /// itself starts at its own default.
    pub fn volume(&self) -> u8 {
        fs::read_to_string(self.root.join("volume")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(40).min(100)
    }

    pub fn save_volume(&self, volume: u8) {
        let _ = fs::write(self.root.join("volume"), volume.to_string());
    }

    pub fn cache(&self, q: Quality) -> PathBuf {
        self.root.join("cache").join(q.cache_name())
    }

    /// The quality asked for last (Lossless when never set).
    pub fn quality(&self) -> Quality {
        fs::read_to_string(self.quality_file()).ok().and_then(|s| Quality::parse(&s)).unwrap_or(Quality::Lossless)
    }

    pub fn set_quality(&self, q: Quality) -> Result<(), String> {
        fs::write(self.quality_file(), q.name()).map_err(|e| format!("cannot store the quality: {e}"))
    }

    /// `--cache-size` for `q`'s cache: the budget less what the other
    /// qualities' caches hold now (never under Soloist's minimum).
    pub fn cache_size_mb(&self, q: Quality) -> u64 {
        let used: u64 = Quality::ALL.into_iter().filter(|&o| o != q).map(|o| dir_size(&self.cache(o))).sum();
        CACHE_BUDGET_MB.saturating_sub(used / (1024 * 1024)).max(CACHE_MIN_MB)
    }

    /// Writes `q`'s tier into the global and `username`'s preferences.
    pub fn apply_quality(&self, q: Quality, username: &str) -> Result<(), String> {
        let settings = self.data().join("settings");
        let tier = q.tier().to_string();
        let keys = [
            ("audio.play_bitrate_enumeration", tier.as_str()),
            ("audio.play_bitrate_non_metered_enumeration", tier.as_str()),
            ("audio.play_bitrate_non_metered_migrated", "true"),
        ];
        for path in [settings.join("prefs"), settings.join("Users").join(format!("{username}-user")).join("prefs")] {
            set_prefs(&path, &keys).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
        Ok(())
    }
}

/// Creates `dir` (and its parents) readable by this user only.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Sets `keys` in the `key=value` file at `path`, keeping its other lines.
fn set_prefs(path: &Path, keys: &[(&str, &str)]) -> std::io::Result<()> {
    let old = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut lines: Vec<String> = old
        .lines()
        .filter(|l| !keys.iter().any(|(k, _)| l.split_once('=').is_some_and(|(lk, _)| lk.trim() == *k)))
        .map(str::to_owned)
        .collect();
    lines.extend(keys.iter().map(|(k, v)| format!("{k}={v}")));
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let tmp = path.with_extension("tern-tmp");
    {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all((lines.join("\n") + "\n").as_bytes())?;
    }
    fs::rename(&tmp, path)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_prefs_replaces_only_its_keys() {
        let dir = std::env::temp_dir().join(format!("tern-prefs-{}", std::process::id()));
        let path = dir.join("Users/u-user/prefs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "storage.size=100\naudio.play_bitrate_enumeration=5\ncore.clock_delta=0\n").unwrap();
        set_prefs(&path, &[("audio.play_bitrate_enumeration", "4"), ("audio.play_bitrate_non_metered_migrated", "true")])
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "storage.size=100\ncore.clock_delta=0\naudio.play_bitrate_enumeration=4\naudio.play_bitrate_non_metered_migrated=true\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
