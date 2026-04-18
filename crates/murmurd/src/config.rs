//! Configuration for murmurd.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Device configuration.
    pub device: DeviceConfig,
    /// Storage paths.
    pub storage: StorageConfig,
    /// Network behaviour.
    #[serde(default)]
    pub network: NetworkConfig,
    /// Folder directory mappings.
    #[serde(default)]
    pub folders: Vec<FolderConfig>,
    /// Per-event notification preferences (M31).
    #[serde(default)]
    pub notifications: NotificationSettings,
}

/// Per-event notification preferences (M31).
///
/// Each flag gates whether a class of events surfaces as a tray notification.
/// Defaults are all-on to match the previous "always notify" behaviour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationSettings {
    /// Notify when a conflict is detected.
    #[serde(default = "default_true")]
    pub conflict: bool,
    /// Notify when a transfer completes.
    #[serde(default = "default_true")]
    pub transfer_completed: bool,
    /// Notify when a new device joins or is approved.
    #[serde(default = "default_true")]
    pub device_joined: bool,
    /// Notify on errors.
    #[serde(default = "default_true")]
    pub error: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            conflict: true,
            transfer_completed: true,
            device_joined: true,
            error: true,
        }
    }
}

impl NotificationSettings {
    /// Return the per-event flag corresponding to an engine event type
    /// (the name used in `EngineEventIpc::event_type`). Unknown events
    /// default to on — if the tray ever grows to notify about new events,
    /// the fail-safe is "surface the event".
    ///
    /// The desktop process performs the same routing client-side today,
    /// but this helper lets daemon-side code (future tray/http bridge)
    /// gate emission before it hits the wire.
    #[allow(dead_code)]
    pub fn is_enabled(&self, event_type: &str) -> bool {
        match event_type {
            "conflict_detected" | "conflict_auto_resolved" => self.conflict,
            "file_synced" | "blob_received" => self.transfer_completed,
            "device_join_requested" | "device_approved" => self.device_joined,
            "error" => self.error,
            _ => true,
        }
    }
}

/// A folder's local directory mapping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderConfig {
    /// The hex-encoded folder ID.
    pub folder_id: String,
    /// Human-readable folder name.
    #[serde(default)]
    pub name: String,
    /// Absolute path to the local directory for this folder.
    pub local_path: PathBuf,
    /// Sync mode: "full", "send-only", or "receive-only".
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Auto-resolve strategy for this folder: "none" (default), "newest", or "mine".
    ///
    /// Used by the conflict-expiry tick (M29): when a conflict ages past
    /// [`Self::conflict_expiry_days`], the configured strategy is applied.
    /// When `"none"`, the daemon falls back to "keep both" — it dismisses
    /// the conflict from the active list without creating a `ConflictResolved`
    /// DAG entry, leaving both versions on disk.
    #[serde(default = "default_auto_resolve")]
    pub auto_resolve: String,
    /// Days until an unresolved conflict is auto-resolved. `None` disables.
    ///
    /// Compared against the conflict's HLC `detected_at` timestamp on each
    /// expiry tick.
    #[serde(default)]
    pub conflict_expiry_days: Option<u64>,
    /// Per-device cosmetic color (hex string, e.g. `"#4f8cff"`). M31.
    ///
    /// Not shared across the network — lives in local config only.
    #[serde(default)]
    pub color_hex: Option<String>,
    /// Per-device cosmetic icon slug or emoji. M31.
    #[serde(default)]
    pub icon: Option<String>,
}

fn default_mode() -> String {
    "full".to_string()
}

fn default_auto_resolve() -> String {
    "none".to_string()
}

/// Device identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceConfig {
    /// Human-readable device name.
    pub name: String,
    /// Legacy role field — ignored, kept for config-file backwards compat.
    #[serde(default, skip_serializing)]
    pub role: Option<String>,
}

/// Paths for persistent storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageConfig {
    /// Directory for content-addressed blobs.
    pub blob_dir: PathBuf,
    /// Directory for DAG storage (`dag.bin`) and Fjall push queue.
    pub data_dir: PathBuf,
}

/// Network behaviour options.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Automatically approve new devices with the correct network ID.
    #[serde(default)]
    pub auto_approve: bool,
    /// Bandwidth throttle configuration.
    #[serde(default)]
    pub throttle: ThrottleConfig,
    /// Enable mDNS LAN peer discovery (on by default).
    #[serde(default = "default_true")]
    pub mdns: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            auto_approve: false,
            throttle: ThrottleConfig::default(),
            mdns: true,
        }
    }
}

/// Bandwidth throttle settings.
///
/// Set to 0 to disable throttling (default).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThrottleConfig {
    /// Maximum upload bytes per second (0 = unlimited).
    #[serde(default)]
    pub max_upload_bytes_per_sec: u64,
    /// Maximum download bytes per second (0 = unlimited).
    #[serde(default)]
    pub max_download_bytes_per_sec: u64,
}

impl Config {
    /// Default base directory: `~/.murmur`.
    pub fn default_base_dir() -> PathBuf {
        dirs_home().join(".murmur")
    }

    /// Create a config rooted at `base_dir`.
    pub fn new(base_dir: &Path, name: &str) -> Self {
        Self {
            device: DeviceConfig {
                name: name.to_string(),
                role: None,
            },
            storage: StorageConfig {
                blob_dir: base_dir.join("blobs"),
                data_dir: base_dir.join("db"),
            },
            network: NetworkConfig::default(),
            folders: Vec::new(),
            notifications: NotificationSettings::default(),
        }
    }

    /// Load config from a TOML file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Save config to a TOML file.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let contents = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Path to the config file within a base directory.
    pub fn config_path(base_dir: &Path) -> PathBuf {
        base_dir.join("config.toml")
    }

    /// Path to the mnemonic file (plaintext for v1).
    pub fn mnemonic_path(base_dir: &Path) -> PathBuf {
        base_dir.join("mnemonic")
    }

    /// Path to the device key file (plaintext for v1).
    pub fn device_key_path(base_dir: &Path) -> PathBuf {
        base_dir.join("device.key")
    }
}

/// Home directory helper (no external dep needed).
fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_roundtrip_toml() {
        let config = Config::new(Path::new("/data/murmur"), "Home NAS");
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn test_config_default_network() {
        let config = Config::new(Path::new("/tmp/test"), "Test");
        assert!(!config.network.auto_approve);
    }

    #[test]
    fn test_config_legacy_role_ignored() {
        // Old config files with a role field should still parse without error.
        let toml = r#"
[device]
name = "NAS"
role = "backup"

[storage]
blob_dir = "/tmp/blobs"
data_dir = "/tmp/db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.device.name, "NAS");
        assert_eq!(config.device.role.as_deref(), Some("backup"));
    }

    #[test]
    fn test_config_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config::new(dir.path(), "NAS");
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(config, loaded);
    }

    #[test]
    fn test_config_paths() {
        let base = Path::new("/home/user/.murmur");
        assert_eq!(Config::config_path(base), base.join("config.toml"));
        assert_eq!(Config::mnemonic_path(base), base.join("mnemonic"));
        assert_eq!(Config::device_key_path(base), base.join("device.key"));
    }

    #[test]
    fn test_config_folder_mappings() {
        let toml_str = r#"
[device]
name = "NAS"
role = "backup"

[storage]
blob_dir = "/data/blobs"
data_dir = "/data/db"

[[folders]]
folder_id = "abc123"
name = "Photos"
local_path = "/home/user/Sync/Photos"
mode = "read-write"

[[folders]]
folder_id = "def456"
name = "Documents"
local_path = "/home/user/Sync/Documents"
mode = "read-only"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.folders.len(), 2);
        assert_eq!(config.folders[0].folder_id, "abc123");
        assert_eq!(config.folders[0].name, "Photos");
        assert_eq!(
            config.folders[0].local_path,
            PathBuf::from("/home/user/Sync/Photos")
        );
        assert_eq!(config.folders[0].mode, "read-write");
        assert_eq!(config.folders[1].name, "Documents");
        assert_eq!(config.folders[1].mode, "read-only");
    }

    #[test]
    fn test_config_folders_default_empty() {
        let toml_str = r#"
[device]
name = "NAS"
role = "backup"

[storage]
blob_dir = "/data/blobs"
data_dir = "/data/db"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.folders.is_empty());
    }

    #[test]
    fn test_config_folder_default_mode() {
        let toml_str = r#"
[device]
name = "NAS"
role = "backup"

[storage]
blob_dir = "/data/blobs"
data_dir = "/data/db"

[[folders]]
folder_id = "abc123"
local_path = "/home/user/Sync/Photos"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.folders[0].mode, "full");
        // name defaults to empty when omitted from TOML (backwards compat).
        assert_eq!(config.folders[0].name, "");
    }

    #[test]
    fn test_config_folder_color_icon_roundtrip() {
        // M31: per-folder cosmetic color/icon persist across save/load.
        let mut config = Config::new(Path::new("/data/murmur"), "Home NAS");
        config.folders.push(FolderConfig {
            folder_id: "aa".repeat(32),
            name: "Photos".to_string(),
            local_path: PathBuf::from("/data/photos"),
            mode: "full".to_string(),
            auto_resolve: "none".to_string(),
            conflict_expiry_days: None,
            color_hex: Some("#4f8cff".to_string()),
            icon: Some("photos".to_string()),
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn test_config_notifications_default_all_on() {
        // M31: absence of a `[notifications]` section defaults to all-on
        // so existing configs keep their current notify-everything behaviour.
        let toml_str = r#"
[device]
name = "NAS"

[storage]
blob_dir = "/data/blobs"
data_dir = "/data/db"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.notifications.conflict);
        assert!(config.notifications.transfer_completed);
        assert!(config.notifications.device_joined);
        assert!(config.notifications.error);
    }

    #[test]
    fn test_notification_settings_is_enabled_routing() {
        // M31: event_type strings route to the right preference flag.
        let settings = NotificationSettings {
            conflict: false,
            transfer_completed: true,
            device_joined: false,
            error: true,
        };
        assert!(!settings.is_enabled("conflict_detected"));
        assert!(!settings.is_enabled("conflict_auto_resolved"));
        assert!(settings.is_enabled("file_synced"));
        assert!(settings.is_enabled("blob_received"));
        assert!(!settings.is_enabled("device_join_requested"));
        assert!(!settings.is_enabled("device_approved"));
        assert!(settings.is_enabled("error"));
        // Unknown events default to on (fail-safe: surface new events).
        assert!(settings.is_enabled("some_new_event"));
    }

    #[test]
    fn test_config_auto_approve() {
        let toml_str = r#"
[device]
name = "NAS"
role = "backup"

[storage]
blob_dir = "/data/blobs"
data_dir = "/data/db"

[network]
auto_approve = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.network.auto_approve);
    }
}
