//! Configuration for murmurd.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

/// A folder's local directory mapping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderConfig {
    /// The hex-encoded folder ID.
    pub folder_id: String,
    /// Absolute path to the local directory for this folder.
    pub local_path: PathBuf,
    /// Sync mode: "read-write" or "read-only".
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "read-write".to_string()
}

/// Device identity and role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceConfig {
    /// Human-readable device name.
    pub name: String,
    /// Device role: "source", "backup", or "full".
    pub role: String,
}

/// Paths for persistent storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageConfig {
    /// Directory for content-addressed blobs.
    pub blob_dir: PathBuf,
    /// Directory for Fjall database (DAG persistence).
    pub data_dir: PathBuf,
}

/// Network behaviour options.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Automatically approve new devices with the correct network ID.
    #[serde(default)]
    pub auto_approve: bool,
    /// Bandwidth throttle configuration.
    #[serde(default)]
    pub throttle: ThrottleConfig,
    /// Enable mDNS LAN peer discovery.
    #[serde(default)]
    pub mdns: bool,
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
    pub fn new(base_dir: &Path, name: &str, role: &str) -> Self {
        Self {
            device: DeviceConfig {
                name: name.to_string(),
                role: role.to_string(),
            },
            storage: StorageConfig {
                blob_dir: base_dir.join("blobs"),
                data_dir: base_dir.join("db"),
            },
            network: NetworkConfig::default(),
            folders: Vec::new(),
        }
    }

    /// Load config from a TOML file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Save config to a TOML file.
    #[cfg(test)]
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

    /// Parse the role string into a [`murmur_types::DeviceRole`].
    pub fn parse_role(&self) -> anyhow::Result<murmur_types::DeviceRole> {
        match self.device.role.as_str() {
            "source" => Ok(murmur_types::DeviceRole::Source),
            "backup" => Ok(murmur_types::DeviceRole::Backup),
            "full" => Ok(murmur_types::DeviceRole::Full),
            other => {
                anyhow::bail!("unknown device role: {other:?} (expected source, backup, or full)")
            }
        }
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
        let config = Config::new(Path::new("/data/murmur"), "Home NAS", "backup");
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn test_config_default_network() {
        let config = Config::new(Path::new("/tmp/test"), "Test", "full");
        assert!(!config.network.auto_approve);
    }

    #[test]
    fn test_config_parse_role() {
        let config = Config::new(Path::new("/tmp"), "NAS", "backup");
        assert_eq!(
            config.parse_role().unwrap(),
            murmur_types::DeviceRole::Backup
        );

        let config = Config::new(Path::new("/tmp"), "Phone", "source");
        assert_eq!(
            config.parse_role().unwrap(),
            murmur_types::DeviceRole::Source
        );

        let config = Config::new(Path::new("/tmp"), "All", "full");
        assert_eq!(config.parse_role().unwrap(), murmur_types::DeviceRole::Full);
    }

    #[test]
    fn test_config_parse_role_invalid() {
        let config = Config::new(Path::new("/tmp"), "NAS", "invalid");
        assert!(config.parse_role().is_err());
    }

    #[test]
    fn test_config_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config::new(dir.path(), "NAS", "backup");
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
local_path = "/home/user/Sync/Photos"
mode = "read-write"

[[folders]]
folder_id = "def456"
local_path = "/home/user/Sync/Documents"
mode = "read-only"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.folders.len(), 2);
        assert_eq!(config.folders[0].folder_id, "abc123");
        assert_eq!(
            config.folders[0].local_path,
            PathBuf::from("/home/user/Sync/Photos")
        );
        assert_eq!(config.folders[0].mode, "read-write");
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
        assert_eq!(config.folders[0].mode, "read-write");
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
