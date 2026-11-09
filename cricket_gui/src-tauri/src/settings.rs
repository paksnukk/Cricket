//! Persistent GUI settings (device names, profile, mode, MSC output address).
//!
//! Devices are stored by **name** (v1.1 goal 7) so a selection survives
//! restarts and re-plugging. Files written before that stored enumeration
//! indices in the same fields; [`load_settings`] maps such indices to the
//! names of the devices currently at those positions, once, and rewrites the
//! file in the new format. An index with no device at that position is
//! dropped (there is no way to know which device it meant).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Profile name as the UI stores it (canonical user-facing name; older
    /// files hold wire-format names, which the UI maps when it loads).
    pub profile: String,
    pub mode: String,
    /// Device / port names as listed by the `get_*` commands; `None` = not set
    /// (audio: first listed device; MIDI: the UI's "None" entry, i.e. no MIDI).
    pub audio_in: Option<String>,
    pub audio_out: Option<String>,
    pub midi_in: Option<String>,
    pub midi_out: Option<String>,
    /// MSC device ID and command format of the decoder's MSC output.
    pub msc_device_id: u8,
    pub msc_command_format: u8,
    /// Audio channels, 1-based (1 = the device's first channel).
    #[serde(default = "default_channel")]
    pub audio_in_channel: u16,
    #[serde(default = "default_channel")]
    pub audio_out_channel: u16,
    /// Encoder transmit level, peak dBFS.
    #[serde(default = "default_tx_level_db")]
    pub tx_level_db: f32,
}

fn default_channel() -> u16 {
    1
}

fn default_tx_level_db() -> f32 {
    cricket_core::phy::DEFAULT_TX_LEVEL_DBFS
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            profile: cricket_core::phy::DEFAULT_PROFILE.into(),
            mode: "both".into(),
            audio_in: None,
            audio_out: None,
            midi_in: None,
            midi_out: None,
            msc_device_id: 0x7F,
            msc_command_format: 0x01,
            audio_in_channel: 1,
            audio_out_channel: 1,
            tx_level_db: default_tx_level_db(),
        }
    }
}

/// A device field as found on disk: a name (current format) or an
/// enumeration index (files written before devices were stored by name).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum StoredDevice {
    Name(String),
    Index(usize),
}

/// The settings file as read from disk, in either format.
#[derive(Debug, Deserialize)]
struct StoredSettings {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    audio_in: Option<StoredDevice>,
    #[serde(default)]
    audio_out: Option<StoredDevice>,
    #[serde(default)]
    midi_in: Option<StoredDevice>,
    #[serde(default)]
    midi_out: Option<StoredDevice>,
    #[serde(default)]
    msc_device_id: Option<u8>,
    #[serde(default)]
    msc_command_format: Option<u8>,
    #[serde(default)]
    audio_in_channel: Option<u16>,
    #[serde(default)]
    audio_out_channel: Option<u16>,
    #[serde(default)]
    tx_level_db: Option<f32>,
}

impl StoredSettings {
    fn has_indices(&self) -> bool {
        [
            &self.audio_in,
            &self.audio_out,
            &self.midi_in,
            &self.midi_out,
        ]
        .iter()
        .any(|d| matches!(d, Some(StoredDevice::Index(_))))
    }
}

/// Current device names, used to migrate index-based settings.
#[derive(Debug, Clone, Default)]
pub struct DeviceNames {
    pub audio_in: Vec<String>,
    pub audio_out: Vec<String>,
    pub midi_in: Vec<String>,
    pub midi_out: Vec<String>,
}

impl DeviceNames {
    /// Enumerate the host's devices now. A list that cannot be read is empty
    /// (its legacy indices are then dropped).
    pub fn current() -> Self {
        use cricket_engine::{audio, midi};
        Self {
            audio_in: audio::list_input_devices().unwrap_or_default(),
            audio_out: audio::list_output_devices().unwrap_or_default(),
            midi_in: midi::list_inputs().unwrap_or_default(),
            midi_out: midi::list_outputs().unwrap_or_default(),
        }
    }
}

fn to_name(device: Option<StoredDevice>, names: &[String]) -> Option<String> {
    match device? {
        StoredDevice::Name(n) if n.is_empty() => None,
        StoredDevice::Name(n) => Some(n),
        StoredDevice::Index(i) => names.get(i).cloned(),
    }
}

/// Outcome of parsing a settings file.
#[derive(Debug, PartialEq)]
pub struct Parsed {
    pub settings: AppSettings,
    /// True if the file used the old index format (and should be rewritten).
    pub migrated: bool,
}

/// Parse settings JSON in either format. `names` is only called when the
/// file contains legacy indices. Unreadable JSON yields the defaults.
pub fn parse_settings(json: &str, names: impl FnOnce() -> DeviceNames) -> Parsed {
    let Ok(stored) = serde_json::from_str::<StoredSettings>(json) else {
        return Parsed {
            settings: AppSettings::default(),
            migrated: false,
        };
    };
    let migrated = stored.has_indices();
    let names = if migrated {
        names()
    } else {
        DeviceNames::default()
    };
    let defaults = AppSettings::default();
    Parsed {
        settings: AppSettings {
            profile: stored.profile.unwrap_or(defaults.profile),
            mode: stored.mode.unwrap_or(defaults.mode),
            audio_in: to_name(stored.audio_in, &names.audio_in),
            audio_out: to_name(stored.audio_out, &names.audio_out),
            midi_in: to_name(stored.midi_in, &names.midi_in),
            midi_out: to_name(stored.midi_out, &names.midi_out),
            msc_device_id: stored
                .msc_device_id
                .filter(|v| *v < 0x80)
                .unwrap_or(defaults.msc_device_id),
            msc_command_format: stored
                .msc_command_format
                .filter(|v| *v < 0x80)
                .unwrap_or(defaults.msc_command_format),
            audio_in_channel: stored.audio_in_channel.filter(|c| *c >= 1).unwrap_or(1),
            audio_out_channel: stored.audio_out_channel.filter(|c| *c >= 1).unwrap_or(1),
            tx_level_db: stored
                .tx_level_db
                .filter(|db| db.is_finite())
                .map(|db| db.clamp(cricket_engine::MIN_TX_LEVEL_DBFS, 0.0))
                .unwrap_or(defaults.tx_level_db),
        },
        migrated,
    }
}

fn settings_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cricket")
}

pub fn settings_path() -> PathBuf {
    settings_dir().join("settings.json")
}

/// Load settings from `path`, migrating (and rewriting) an index-based file.
pub fn load_settings_from(path: &Path, names: impl FnOnce() -> DeviceNames) -> AppSettings {
    let Ok(json) = fs::read_to_string(path) else {
        return AppSettings::default();
    };
    let parsed = parse_settings(&json, names);
    if parsed.migrated {
        // Best effort: if the write fails, the migration repeats next time.
        let _ = save_settings_to(path, &parsed.settings);
    }
    parsed.settings
}

pub fn save_settings_to(path: &Path, settings: &AppSettings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

pub fn load_settings() -> AppSettings {
    load_settings_from(&settings_path(), DeviceNames::current)
}

pub fn save_settings(settings: &AppSettings) -> Result<(), String> {
    save_settings_to(&settings_path(), settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> DeviceNames {
        DeviceNames {
            audio_in: vec!["Built-in Mic".into(), "USB Interface".into()],
            audio_out: vec!["Speakers".into(), "USB Interface".into()],
            midi_in: vec!["Midi Through".into()],
            midi_out: vec!["Midi Through".into(), "UM-ONE".into()],
        }
    }

    const OLD: &str = r#"{
        "profile": "fast",
        "mode": "decode",
        "audio_in": 1,
        "audio_out": 0,
        "midi_in": null,
        "midi_out": 1
    }"#;

    #[test]
    fn migrates_old_index_json_to_names() {
        let parsed = parse_settings(OLD, names);
        assert!(parsed.migrated);
        assert_eq!(
            parsed.settings,
            AppSettings {
                profile: "fast".into(),
                mode: "decode".into(),
                audio_in: Some("USB Interface".into()),
                audio_out: Some("Speakers".into()),
                midi_in: None,
                midi_out: Some("UM-ONE".into()),
                msc_device_id: 0x7F,
                msc_command_format: 0x01,
                audio_in_channel: 1,
                audio_out_channel: 1,
                tx_level_db: -12.0,
            }
        );
        // The new JSON stores names.
        let json: serde_json::Value = serde_json::to_value(&parsed.settings).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "profile": "fast",
                "mode": "decode",
                "audio_in": "USB Interface",
                "audio_out": "Speakers",
                "midi_in": null,
                "midi_out": "UM-ONE",
                "msc_device_id": 127,
                "msc_command_format": 1,
                "audio_in_channel": 1,
                "audio_out_channel": 1,
                "tx_level_db": -12.0
            })
        );
    }

    #[test]
    fn index_without_device_is_dropped() {
        let json = r#"{"profile":"robust","mode":"both","audio_in":5,"audio_out":null,"midi_in":0,"midi_out":null}"#;
        let parsed = parse_settings(json, names);
        assert!(parsed.migrated);
        assert_eq!(parsed.settings.audio_in, None);
        assert_eq!(parsed.settings.midi_in, Some("Midi Through".into()));
    }

    #[test]
    fn new_format_is_read_as_is_without_enumerating() {
        let settings = AppSettings {
            profile: "robust".into(),
            mode: "both".into(),
            audio_in: Some("Gone Device".into()),
            audio_out: None,
            midi_in: Some("UM-ONE".into()),
            midi_out: None,
            msc_device_id: 3,
            msc_command_format: 0x10,
            audio_in_channel: 3,
            audio_out_channel: 2,
            tx_level_db: -18.0,
        };
        let json = serde_json::to_string(&settings).unwrap();
        let parsed = parse_settings(&json, || panic!("must not enumerate devices"));
        assert!(!parsed.migrated);
        // A missing device keeps its name (the UI warns; nothing else is picked).
        assert_eq!(parsed.settings, settings);
    }

    #[test]
    fn garbage_and_partial_files() {
        let parsed = parse_settings("not json", || panic!());
        assert_eq!(parsed.settings, AppSettings::default());
        assert!(!parsed.migrated);
        let parsed = parse_settings(r#"{"profile":"fast"}"#, || panic!());
        assert_eq!(parsed.settings.profile, "fast");
        assert_eq!(parsed.settings.mode, "both");
        assert_eq!(parsed.settings.audio_in_channel, 1);
        assert_eq!(parsed.settings.tx_level_db, -12.0);
        // Out-of-range values fall back to safe ones.
        let parsed = parse_settings(
            r#"{"audio_in_channel":0,"audio_out_channel":4,"tx_level_db":6.0}"#,
            || panic!(),
        );
        assert_eq!(parsed.settings.audio_in_channel, 1);
        assert_eq!(parsed.settings.audio_out_channel, 4);
        assert_eq!(parsed.settings.tx_level_db, 0.0);
    }

    #[test]
    fn load_rewrites_migrated_file_once() {
        let dir = std::env::temp_dir().join(format!(
            "cricket-settings-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("settings.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, OLD).unwrap();

        let first = load_settings_from(&path, names);
        assert_eq!(first.midi_out.as_deref(), Some("UM-ONE"));
        let on_disk: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk["audio_in"], "USB Interface");
        // Second load: new format, no enumeration, same result.
        let second = load_settings_from(&path, || panic!("already migrated"));
        assert_eq!(second, first);
        let _ = fs::remove_dir_all(&dir);
    }
}
