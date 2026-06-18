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

use crate::{BRICK_DIR, CURRENT_SOURCE_FILE, SOURCE_PROFILES_DIR};

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
            notes: None,
        }
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

    /// Returns the directory containing source profile JSON files.
    pub fn profiles_dir(&self) -> PathBuf {
        self.config_dir().join(SOURCE_PROFILES_DIR)
    }

    /// Writes or replaces a source profile.
    pub fn write_profile(&self, profile: &SourceProfile) -> Result<()> {
        validate_profile_name(&profile.name)?;
        fs::create_dir_all(self.profiles_dir())
            .context("failed to create source profiles directory")?;
        let path = self.profile_path(&profile.name)?;
        let serialized =
            serde_json::to_string_pretty(profile).context("failed to serialize source profile")?;
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
        let profile = serde_json::from_str(&contents)
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
                .is_some_and(|extension| extension == "json")
            {
                paths.push(path);
            }
        }
        paths.sort();

        let mut profiles = Vec::new();
        for path in paths {
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read source profile at {}", path.display()))?;
            let profile: SourceProfile = serde_json::from_str(&contents)
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
        let serialized = serde_json::to_string_pretty(&selected)
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
        let selected: SelectedSourceProfile = serde_json::from_str(&contents)
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

    fn current_source_path(&self) -> PathBuf {
        self.config_dir().join(CURRENT_SOURCE_FILE)
    }

    fn profile_path(&self, name: &str) -> Result<PathBuf> {
        validate_profile_name(name)?;
        Ok(self.profiles_dir().join(format!("{name}.json")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SelectedSourceProfile {
    name: String,
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
}
