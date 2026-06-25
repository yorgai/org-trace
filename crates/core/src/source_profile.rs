//! Repository-local source profile storage.
//!
//! Profiles describe external tools and actors that feed Brick. The profile
//! registry is kept under the repository `.brick` directory instead of the
//! effective event store root so it can be read before a profile selects a
//! custom store location.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use brick_protocol::ActorType;
use serde::{Deserialize, Serialize};

use crate::{BRICK_CONFIG_FILE, BRICK_DIR, CURRENT_SOURCE_FILE, SOURCE_PROFILES_DIR};

/// Repository-level Brick behavior that must be known before source selection.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BrickConfig {
    #[serde(default)]
    pub evidence: EvidenceConfig,
}

/// Evidence defaults for local capture and upload behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceConfig {
    #[serde(default)]
    pub default_full_evidence_upload: bool,
    #[serde(default = "default_metadata_only_local")]
    pub metadata_only_local: bool,
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            default_full_evidence_upload: false,
            metadata_only_local: true,
        }
    }
}

/// Default identity and storage hints for a provenance source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProfile {
    pub name: String,
    pub app_id: Option<String>,
    pub actor_id: Option<String>,
    pub actor_type: Option<ActorType>,
    pub store_root: Option<PathBuf>,
    pub session_db_path: Option<PathBuf>,
    pub session_log_path: Option<PathBuf>,
    pub evidence_root: Option<PathBuf>,
    pub cursor_state_db_path: Option<PathBuf>,
    #[serde(default)]
    pub default_full_evidence_upload: Option<bool>,
    pub notes: Option<String>,
}

impl SourceProfile {
    /// Creates an empty profile with a required unique name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            app_id: None,
            actor_id: None,
            actor_type: None,
            store_root: None,
            session_db_path: None,
            session_log_path: None,
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        }
    }

    /// Returns whether this source opts into copying/uploading full evidence by default.
    pub fn should_upload_full_evidence(&self, config: &BrickConfig) -> bool {
        self.default_full_evidence_upload
            .unwrap_or(config.evidence.default_full_evidence_upload)
    }
}

/// Repository-local profile registry used during store bootstrap.
#[derive(Debug, Clone)]
pub struct SourceProfileStore {
    repo_root: PathBuf,
}

impl SourceProfileStore {
    /// Creates a profile registry rooted at the Git repository.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// Returns the repository root that owns this profile registry.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Returns the repo-local `.brick` config directory.
    pub fn config_dir(&self) -> PathBuf {
        self.repo_root.join(BRICK_DIR)
    }

    /// Returns the directory containing source profile TOML files.
    pub fn profiles_dir(&self) -> PathBuf {
        self.config_dir().join(SOURCE_PROFILES_DIR)
    }

    /// Writes the repo-level config file.
    pub fn write_config(&self, config: &BrickConfig) -> Result<()> {
        fs::create_dir_all(self.config_dir()).context("failed to create Brick config directory")?;
        let path = self.config_path();
        let serialized =
            toml::to_string_pretty(config).context("failed to serialize Brick config")?;
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write Brick config at {}", path.display()))?;
        Ok(())
    }

    /// Reads the repo-level config file or returns defaults when it does not exist.
    pub fn read_config(&self) -> Result<BrickConfig> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(BrickConfig::default());
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read Brick config at {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse Brick config at {}", path.display()))
    }

    /// Writes or replaces a source profile.
    pub fn write_profile(&self, profile: &SourceProfile) -> Result<()> {
        validate_profile_name(&profile.name)?;
        fs::create_dir_all(self.profiles_dir())
            .context("failed to create source profiles directory")?;
        let path = self.profile_path(&profile.name)?;
        let serialized =
            toml::to_string_pretty(profile).context("failed to serialize source profile")?;
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write source profile at {}", path.display()))?;
        Ok(())
    }

    /// Reads one profile by name.
    pub fn read_profile(&self, name: &str) -> Result<Option<SourceProfile>> {
        validate_profile_name(name)?;
        let path = self.profile_path(name)?;
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read source profile at {}", path.display()))?;
        let profile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse source profile at {}", path.display()))?;
        Ok(Some(profile))
    }

    /// Lists all stored profiles in deterministic name order.
    pub fn list_profiles(&self) -> Result<Vec<SourceProfile>> {
        if !self.profiles_dir().exists() {
            return Ok(Vec::new());
        }

        let mut paths = Vec::new();
        for entry in fs::read_dir(self.profiles_dir()).with_context(|| {
            format!(
                "failed to read source profiles directory {}",
                self.profiles_dir().display()
            )
        })? {
            let entry = entry.context("failed to read source profile directory entry")?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "toml")
            {
                paths.push(path);
            }
        }
        paths.sort();

        let mut profiles = Vec::new();
        for path in paths {
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read source profile at {}", path.display()))?;
            let profile: SourceProfile = toml::from_str(&contents)
                .with_context(|| format!("failed to parse source profile at {}", path.display()))?;
            profiles.push(profile);
        }
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(profiles)
    }

    /// Persists the source profile selected by default for this repository.
    pub fn use_profile(&self, name: &str) -> Result<SourceProfile> {
        let profile = self
            .read_profile(name)?
            .ok_or_else(|| anyhow!("source profile not found: {name}"))?;
        fs::create_dir_all(self.config_dir()).context("failed to create Brick config directory")?;
        let selected = SelectedSourceProfile {
            name: profile.name.clone(),
        };
        let path = self.current_source_path();
        let serialized = toml::to_string_pretty(&selected)
            .context("failed to serialize selected source profile")?;
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write selected source at {}", path.display()))?;
        Ok(profile)
    }

    /// Reads the selected source profile name, if one is configured.
    pub fn selected_profile_name(&self) -> Result<Option<String>> {
        let path = self.current_source_path();
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read selected source at {}", path.display()))?;
        let selected: SelectedSourceProfile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse selected source at {}", path.display()))?;
        Ok(Some(selected.name))
    }

    /// Loads the explicitly requested profile or the repo-selected profile.
    pub fn selected_profile(&self, requested_name: Option<&str>) -> Result<Option<SourceProfile>> {
        let Some(name) = requested_name
            .map(ToOwned::to_owned)
            .or(self.selected_profile_name()?)
        else {
            return Ok(None);
        };
        self.read_profile(&name)?
            .ok_or_else(|| anyhow!("source profile not found: {name}"))
            .map(Some)
    }

    fn config_path(&self) -> PathBuf {
        self.config_dir().join(BRICK_CONFIG_FILE)
    }

    fn current_source_path(&self) -> PathBuf {
        self.config_dir().join(CURRENT_SOURCE_FILE)
    }

    fn profile_path(&self, name: &str) -> Result<PathBuf> {
        validate_profile_name(name)?;
        Ok(self.profiles_dir().join(format!("{name}.toml")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SelectedSourceProfile {
    name: String,
}

fn default_metadata_only_local() -> bool {
    true
}

fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("source profile name cannot be empty"));
    }
    if name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Ok(())
    } else {
        Err(anyhow!(
            "source profile name may only contain ASCII letters, numbers, hyphen, or underscore"
        ))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn temp_repo_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-test-profile-{name}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(path.join(".git")).expect("create fake git dir");
        path
    }

    #[test]
    fn config_defaults_to_metadata_only_local() {
        let repo_root = temp_repo_root("config-default");
        let registry = SourceProfileStore::new(&repo_root);

        assert_eq!(
            registry.read_config().expect("read default config"),
            BrickConfig::default()
        );
    }

    #[test]
    fn config_round_trips_as_toml() {
        let repo_root = temp_repo_root("config-roundtrip");
        let registry = SourceProfileStore::new(&repo_root);
        let config = BrickConfig {
            evidence: EvidenceConfig {
                default_full_evidence_upload: true,
                metadata_only_local: true,
            },
        };

        registry.write_config(&config).expect("write config");

        assert_eq!(registry.read_config().expect("read config"), config);
        assert!(registry.config_path().ends_with("config.toml"));
    }

    #[test]
    fn profile_round_trips_and_can_be_selected() {
        let repo_root = temp_repo_root("roundtrip");
        let registry = SourceProfileStore::new(&repo_root);
        let profile = SourceProfile {
            name: "cursor".to_string(),
            app_id: Some("cursor".to_string()),
            actor_id: Some("agent-1".to_string()),
            actor_type: Some(ActorType::Agent),
            store_root: Some(PathBuf::from("../shared-store")),
            session_db_path: Some(PathBuf::from("sessions.db")),
            session_log_path: Some(PathBuf::from("sessions.jsonl")),
            evidence_root: Some(PathBuf::from(".orgii")),
            cursor_state_db_path: Some(PathBuf::from(
                "~/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
            )),
            default_full_evidence_upload: Some(false),
            notes: Some("local agent".to_string()),
        };

        registry.write_profile(&profile).expect("write profile");
        let read = registry
            .read_profile("cursor")
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(profile, read);

        registry.use_profile("cursor").expect("select profile");
        assert_eq!(
            registry
                .selected_profile_name()
                .expect("selected profile name"),
            Some("cursor".to_string())
        );
        assert_eq!(
            registry
                .selected_profile(None)
                .expect("selected profile")
                .expect("profile exists"),
            profile
        );
    }

    #[test]
    fn source_profile_overrides_full_evidence_default() {
        let mut profile = SourceProfile::named("cursor");
        let config = BrickConfig {
            evidence: EvidenceConfig {
                default_full_evidence_upload: true,
                metadata_only_local: true,
            },
        };

        assert!(profile.should_upload_full_evidence(&config));
        profile.default_full_evidence_upload = Some(false);
        assert!(!profile.should_upload_full_evidence(&config));
    }
}
