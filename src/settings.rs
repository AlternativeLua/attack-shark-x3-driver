//! The user-facing settings, their validation, and the file they are saved in.
//!
//! The mouse itself is the source of truth. The file is a copy of what was
//! last read from or written to it: a profile `x3ctl apply` can restore, and
//! the fallback when the mouse cannot be read.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

pub const STAGES: usize = 6;
pub const DPI_STEP: u32 = 50;
pub const DPI_MAX: u32 = 26_000;

/// Which feature report a setting lives in. Returned by [`Settings::set`] so
/// the caller knows what has to be resent.
pub mod group {
    pub const SENSOR: u8 = 1;
    pub const POWER: u8 = 2;
    pub const POLLING: u8 = 4;
    pub const ALL: u8 = SENSOR | POWER | POLLING;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub dpi: [u32; STAGES],
    /// Active DPI stage, 1-based.
    pub stage: u8,
    /// Polling rate in Hz.
    pub polling: u16,
    /// Lift-off distance in mm (1 or 2).
    pub lod: u8,
    pub ripple: bool,
    pub angle_snap: bool,
    pub motion_sync: bool,
    /// Idle time before sleep, in half minutes (the mouse's own unit).
    pub sleep_half_min: u8,
    /// Idle time before deep sleep, in minutes.
    pub deep_sleep: u8,
    /// Button debounce ("key response time") in ms.
    pub debounce: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dpi: [800, 1600, 2400, 3200, 5000, 10_000],
            stage: 2,
            polling: 1000,
            lod: 1,
            ripple: false,
            angle_snap: false,
            motion_sync: false,
            sleep_half_min: 4,
            deep_sleep: 10,
            debounce: 6,
        }
    }
}

impl Settings {
    /// Validates and applies one `key = value` pair, as used by both the
    /// command line and the settings file. Returns the affected [`group`].
    pub fn set(&mut self, key: &str, value: &str) -> Result<u8, String> {
        let key = key.replace('_', "-");
        let v = value.trim();
        match key.as_str() {
            "dpi" => {
                self.dpi[self.stage as usize - 1] = parse_dpi(v)?;
                Ok(group::SENSOR)
            }
            "dpis" => {
                let parts: Vec<&str> = v
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|p| !p.is_empty())
                    .collect();
                if parts.len() != STAGES {
                    return Err(format!("expected {STAGES} values, got {}", parts.len()));
                }
                let mut dpi = [0; STAGES];
                for (slot, part) in dpi.iter_mut().zip(parts) {
                    *slot = parse_dpi(part)?;
                }
                self.dpi = dpi;
                Ok(group::SENSOR)
            }
            "stage" => {
                self.stage = parse_stage(v)?;
                Ok(group::SENSOR)
            }
            "polling" => {
                self.polling = match v.trim_end_matches("hz").trim_end_matches("Hz") {
                    "125" => 125,
                    "250" => 250,
                    "500" => 500,
                    "1000" => 1000,
                    _ => return Err(format!("`{v}` is not one of 125, 250, 500, 1000")),
                };
                Ok(group::POLLING)
            }
            "lod" => {
                self.lod = match v.trim_end_matches("mm") {
                    "1" => 1,
                    "2" => 2,
                    _ => return Err(format!("`{v}` is not 1 or 2 (mm)")),
                };
                Ok(group::SENSOR)
            }
            "ripple" => {
                self.ripple = parse_bool(v)?;
                Ok(group::SENSOR)
            }
            "angle-snap" => {
                self.angle_snap = parse_bool(v)?;
                Ok(group::SENSOR)
            }
            "motion-sync" => {
                self.motion_sync = parse_bool(v)?;
                Ok(group::SENSOR)
            }
            "sleep" => {
                let half = v
                    .parse::<f64>()
                    .map_err(|_| format!("`{v}` is not a number"))?
                    * 2.0;
                if half.fract() != 0.0 || !(1.0..=60.0).contains(&half) {
                    return Err(format!("`{v}` is not 0.5..30 minutes in steps of 0.5"));
                }
                self.sleep_half_min = half as u8;
                Ok(group::POWER)
            }
            "deep-sleep" => {
                self.deep_sleep = match v.parse() {
                    Ok(m @ 1..=60) => m,
                    _ => return Err(format!("`{v}` is not 1..60 minutes")),
                };
                Ok(group::POWER)
            }
            "debounce" | "key-response" => {
                self.debounce = match v.trim_end_matches("ms").parse() {
                    Ok(ms @ 4..=50) if ms % 2 == 0 => ms,
                    _ => return Err(format!("`{v}` is not an even number of ms in 4..50")),
                };
                Ok(group::POWER)
            }
            _ => match key.strip_prefix("dpi").map(parse_stage) {
                Some(Ok(stage)) => {
                    self.dpi[stage as usize - 1] = parse_dpi(v)?;
                    Ok(group::SENSOR)
                }
                _ => Err("unknown setting".into()),
            },
        }
    }

    pub fn sleep_minutes(&self) -> String {
        match self.sleep_half_min {
            h if h % 2 == 0 => format!("{}", h / 2),
            h => format!("{}.5", h / 2),
        }
    }

    /// Human-readable listing for `x3ctl show`.
    pub fn summary(&self) -> String {
        let mut out = String::from("DPI stages  ");
        for (i, dpi) in self.dpi.iter().enumerate() {
            if i + 1 == self.stage as usize {
                let _ = write!(out, " [{dpi}]");
            } else {
                let _ = write!(out, " {dpi}");
            }
        }
        let _ = writeln!(out, "   (stage {} active)", self.stage);
        let _ = writeln!(out, "Polling      {} Hz", self.polling);
        let _ = writeln!(out, "Lift-off     {} mm", self.lod);
        let _ = writeln!(out, "Ripple       {}", on_off(self.ripple));
        let _ = writeln!(out, "Angle snap   {}", on_off(self.angle_snap));
        let _ = writeln!(out, "Motion sync  {}", on_off(self.motion_sync));
        let _ = writeln!(out, "Sleep        {} min", self.sleep_minutes());
        let _ = writeln!(out, "Deep sleep   {} min", self.deep_sleep);
        let _ = write!(out, "Debounce     {} ms", self.debounce);
        out
    }

    fn to_file(&self) -> String {
        let dpis: Vec<String> = self.dpi.iter().map(u32::to_string).collect();
        format!(
            "# x3ctl's copy of the mouse's settings, as last read or written.\n\
             dpis = {}\n\
             stage = {}\n\
             polling = {}\n\
             lod = {}\n\
             ripple = {}\n\
             angle-snap = {}\n\
             motion-sync = {}\n\
             sleep = {}\n\
             deep-sleep = {}\n\
             debounce = {}\n",
            dpis.join(" "),
            self.stage,
            self.polling,
            self.lod,
            on_off(self.ripple),
            on_off(self.angle_snap),
            on_off(self.motion_sync),
            self.sleep_minutes(),
            self.deep_sleep,
            self.debounce,
        )
    }
}

fn parse_dpi(v: &str) -> Result<u32, String> {
    match v.parse::<u32>() {
        Ok(dpi) if (DPI_STEP..=DPI_MAX).contains(&dpi) && dpi % DPI_STEP == 0 => Ok(dpi),
        _ => Err(format!(
            "`{v}` is not a DPI in {DPI_STEP}..{DPI_MAX} in steps of {DPI_STEP}"
        )),
    }
}

fn parse_stage(v: &str) -> Result<u8, String> {
    match v.parse::<u8>() {
        Ok(stage) if (1..=STAGES as u8).contains(&stage) => Ok(stage),
        _ => Err(format!("`{v}` is not a stage in 1..{STAGES}")),
    }
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(format!("`{v}` is not on or off")),
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// `$XDG_CONFIG_HOME/x3ctl/settings.conf`, falling back to `~/.config`.
pub fn default_path() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => match env::var_os("HOME").filter(|v| !v.is_empty()) {
            Some(home) => PathBuf::from(home).join(".config"),
            None => return Err("neither XDG_CONFIG_HOME nor HOME is set; pass --config".into()),
        },
    };
    Ok(base.join("x3ctl/settings.conf"))
}

/// Returns `None` when nothing has been saved yet.
pub fn load(path: &Path) -> Result<Option<Settings>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    parse(&text)
        .map(Some)
        .map_err(|e| format!("{}:{e}", path.display()))
}

/// Parses `key = value` lines; errors are prefixed with the line number.
fn parse(text: &str) -> Result<Settings, String> {
    let mut settings = Settings::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("{}: expected `key = value`", n + 1))?;
        let key = key.trim();
        settings
            .set(key, value)
            .map_err(|e| format!("{}: {key}: {e}", n + 1))?;
    }
    Ok(settings)
}

pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    let write = || -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, settings.to_file())
    };
    write().map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_validates_and_reports_group() {
        let mut s = Settings::default();
        assert_eq!(s.set("stage", "3"), Ok(group::SENSOR));
        assert_eq!(s.set("dpi", "1200"), Ok(group::SENSOR));
        assert_eq!(s.dpi[2], 1200);
        assert_eq!(s.set("dpi6", "26000"), Ok(group::SENSOR));
        assert_eq!(s.dpi[5], 26_000);
        assert_eq!(
            s.set("dpis", "400,800 1600, 3200 6400 12800"),
            Ok(group::SENSOR)
        );
        assert_eq!(s.dpi, [400, 800, 1600, 3200, 6400, 12_800]);
        assert_eq!(s.set("polling", "500"), Ok(group::POLLING));
        assert_eq!(s.set("sleep", "4.5"), Ok(group::POWER));
        assert_eq!(s.sleep_half_min, 9);
        assert_eq!(s.set("angle_snap", "on"), Ok(group::SENSOR));
        assert!(s.angle_snap);

        for (key, value) in [
            ("dpi", "1025"),
            ("dpi", "0"),
            ("dpi", "26050"),
            ("dpi7", "800"),
            ("dpis", "800 1600"),
            ("stage", "0"),
            ("stage", "7"),
            ("polling", "2000"),
            ("lod", "3"),
            ("ripple", "maybe"),
            ("sleep", "0.25"),
            ("sleep", "31"),
            ("deep-sleep", "0"),
            ("deep-sleep", "61"),
            ("debounce", "5"),
            ("debounce", "52"),
            ("colour", "red"),
        ] {
            assert!(
                s.set(key, value).is_err(),
                "{key}={value} should be rejected"
            );
        }
    }

    #[test]
    fn rejected_dpis_leave_settings_untouched() {
        let mut s = Settings::default();
        let before = s.clone();
        assert!(s.set("dpis", "400 800 1600 3200 6400 99999").is_err());
        assert_eq!(s, before);
    }

    #[test]
    fn file_round_trips() {
        let mut s = Settings::default();
        s.set("dpis", "400 800 1600 3200 6400 26000").unwrap();
        s.set("stage", "5").unwrap();
        s.set("polling", "250").unwrap();
        s.set("lod", "2").unwrap();
        s.set("motion-sync", "on").unwrap();
        s.set("sleep", "8.5").unwrap();
        s.set("deep-sleep", "27").unwrap();
        s.set("debounce", "24").unwrap();

        assert_eq!(parse(&s.to_file()), Ok(s));
    }

    #[test]
    fn parse_errors_name_the_line() {
        assert_eq!(
            parse("# comment\npolling = 1000\nstage = 9\n"),
            Err("3: stage: `9` is not a stage in 1..6".into())
        );
        assert_eq!(parse("polling\n"), Err("1: expected `key = value`".into()));
    }
}
