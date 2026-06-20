use crate::subscription::SubscriptionProfile;
use crate::vpn::VpnError;
use std::path::PathBuf;

pub struct ProfileManager {
    profiles: Vec<SubscriptionProfile>,
    file_path: PathBuf,
}

impl ProfileManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            profiles: Vec::new(),
            file_path: data_dir.join("profiles.json"),
        }
    }

    pub fn load(&mut self) -> Result<(), VpnError> {
        match std::fs::read_to_string(&self.file_path) {
            Ok(json) => {
                let profiles: Vec<SubscriptionProfile> =
                    serde_json::from_str(&json).map_err(|e| {
                        VpnError::Subscription(format!("Failed to parse profiles.json: {e}"))
                    })?;
                self.profiles = profiles;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First launch — no profiles yet
                self.profiles = Vec::new();
                Ok(())
            }
            Err(e) => Err(VpnError::Io(e)),
        }
    }

    fn save(&self) -> Result<(), VpnError> {
        let json = serde_json::to_string_pretty(&self.profiles).map_err(|e| {
            VpnError::Subscription(format!("Failed to serialize profiles: {e}"))
        })?;
        std::fs::write(&self.file_path, json)?;
        Ok(())
    }

    pub fn list(&self) -> &[SubscriptionProfile] {
        &self.profiles
    }

    pub async fn add(&mut self, name: String, url: String) -> Result<SubscriptionProfile, VpnError> {
        let clean_url = url.trim().to_string();

        if self.profiles.iter().any(|p| p.url == clean_url) {
            return Err(VpnError::Subscription(format!(
                "Subscription with URL '{}' already exists",
                clean_url
            )));
        }

        let servers = if clean_url.starts_with("http://") || clean_url.starts_with("https://") {
            crate::subscription::fetch_subscription(&clean_url).await?
        } else {
            crate::subscription::parser::parse_subscription(&clean_url)?
        };

        let profile = SubscriptionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            url: clean_url,
            servers,
            updated_at: chrono::Utc::now().to_rfc3339(),
        };

        self.profiles.push(profile.clone());
        self.save()?;
        Ok(profile)
    }

    pub fn remove(&mut self, id: &str) -> Result<(), VpnError> {
        let len_before = self.profiles.len();
        self.profiles.retain(|p| p.id != id);
        if self.profiles.len() == len_before {
            return Err(VpnError::Subscription("Profile not found".into()));
        }
        self.save()?;
        Ok(())
    }

    pub async fn refresh(&mut self, id: &str) -> Result<SubscriptionProfile, VpnError> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| VpnError::Subscription("Profile not found".into()))?;

        let servers = crate::subscription::fetch_subscription(&self.profiles[idx].url).await?;
        self.profiles[idx].servers = servers;
        self.profiles[idx].updated_at = chrono::Utc::now().to_rfc3339();
        let profile = self.profiles[idx].clone();
        self.save()?;
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "omnitun-profile-test-{}-{}",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn test_new_creates_empty_manager() {
        let mgr = ProfileManager::new(PathBuf::from("/tmp/test"));
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn test_load_nonexistent_file_returns_empty() {
        let dir = test_dir("nonexistent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut mgr = ProfileManager::new(dir.clone());
        mgr.load().unwrap();
        assert!(mgr.list().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = test_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let profile = SubscriptionProfile {
            id: "test-1".into(),
            name: "Test".into(),
            url: "https://example.com/sub".into(),
            servers: vec![],
            updated_at: "2025-01-01T00:00:00Z".into(),
        };

        {
            let mut mgr = ProfileManager::new(dir.clone());
            mgr.profiles.push(profile);
            mgr.save().unwrap();
        }

        {
            let mut mgr = ProfileManager::new(dir.clone());
            mgr.load().unwrap();
            assert_eq!(mgr.list().len(), 1);
            assert_eq!(mgr.list()[0].name, "Test");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_remove_nonexistent_returns_error() {
        let dir = test_dir("remove_err");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut mgr = ProfileManager::new(dir.clone());
        let result = mgr.remove("nonexistent");
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
