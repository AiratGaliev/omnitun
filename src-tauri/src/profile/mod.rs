use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::subscription::SubscriptionProfile;
use crate::vpn::{ServerProfile, VpnError};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tokio::sync::Semaphore;

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

    pub async fn add(&mut self, app: &AppHandle, name: String, url: String) -> Result<SubscriptionProfile, VpnError> {
        let clean_url = url.trim().to_string();

        if self.profiles.iter().any(|p| p.url == clean_url) {
            return Err(VpnError::Subscription(format!(
                "Subscription with URL '{}' already exists",
                clean_url
            )));
        }

        let mut servers = if clean_url.starts_with("http://") || clean_url.starts_with("https://") {
            crate::subscription::fetch_subscription(&clean_url).await?
        } else {
            crate::subscription::parser::parse_subscription(&clean_url)?
        };

        servers = process_servers(app, servers).await;

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

    pub async fn refresh(&mut self, app: &AppHandle, id: &str) -> Result<SubscriptionProfile, VpnError> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| VpnError::Subscription("Profile not found".into()))?;

        let mut servers = crate::subscription::fetch_subscription(&self.profiles[idx].url).await?;

        // Preserve server IDs across refresh — match by (protocol, server, port, uuid, password)
        let old_index: std::collections::HashMap<String, String> = self.profiles[idx]
            .servers
            .iter()
            .map(|s| {
                let key = format!(
                    "{}://{}:{}@{}:{}",
                    s.protocol,
                    s.server,
                    s.port,
                    s.uuid.as_deref().unwrap_or(""),
                    s.password.as_deref().unwrap_or(""),
                );
                (key, s.id.clone())
            })
            .collect();
        for server in &mut servers {
            let key = format!(
                "{}://{}:{}@{}:{}",
                server.protocol,
                server.server,
                server.port,
                server.uuid.as_deref().unwrap_or(""),
                server.password.as_deref().unwrap_or(""),
            );
            if let Some(old_id) = old_index.get(&key) {
                server.id = old_id.clone();
            }
        }

        servers = process_servers(app, servers).await;
        self.profiles[idx].servers = servers;
        self.profiles[idx].updated_at = chrono::Utc::now().to_rfc3339();
        let profile = self.profiles[idx].clone();
        self.save()?;
        Ok(profile)
    }
}

/// Remove duplicate servers and test reachability via TCP ping.
/// Logs each step (dedup count, testing, unreachable count, final count).
pub async fn process_servers(app: &AppHandle, servers: Vec<ServerProfile>) -> Vec<ServerProfile> {
    let initial_count = servers.len();
    let logger = app.state::<Mutex<Logger>>();

    // 1. Deduplication by protocol + server + port + uuid + password
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for s in servers {
        let key = format!(
            "{}://{}:{}@{}:{}",
            s.protocol,
            s.server,
            s.port,
            s.uuid.as_deref().unwrap_or(""),
            s.password.as_deref().unwrap_or(""),
        );
        if seen.insert(key) {
            unique.push(s);
        }
    }
    let duplicates = initial_count - unique.len();
    if duplicates > 0 {
        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Subscription,
            format!("Removed {duplicates} duplicate(s)"),
        );
    }

    // 2. TCP ping — up to 50 concurrent connections, 3-second timeout each
    let unique_count = unique.len();
    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Testing {unique_count} server(s) for availability..."),
    );

    let semaphore = Arc::new(Semaphore::new(50));
    let mut set = tokio::task::JoinSet::new();

    for mut server in unique {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore not closed");

        // Resolve country flag from server IP (if not already set)
        if server.flag.is_none() {
            let country = crate::vpn::geoip::resolve(&server.server);
            if country != "Unknown" {
                server.flag = Some(country);
            }
        }

        set.spawn(async move {
            let addr = format!("{}:{}", server.server, server.port);
            let start = std::time::Instant::now();
            let reachable = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::net::TcpStream::connect(&addr),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            let ping_ms = start.elapsed().as_millis() as u64;
            drop(permit);
            (server, reachable, ping_ms)
        });
    }

    let mut alive = Vec::new();
    while let Some(result) = set.join_next().await {
        if let Ok((mut server, true, ping_ms)) = result {
            server.ping_ms = Some(ping_ms);
            alive.push(server);
        }
    }

    let unreachable = unique_count - alive.len();
    if unreachable > 0 {
        log(
            app,
            &logger,
            LogLevel::Warning,
            LogSource::Subscription,
            format!("Removed {unreachable} unreachable server(s)"),
        );
    }

    log(
        app,
        &logger,
        LogLevel::Success,
        LogSource::Subscription,
        format!("{} server(s) passed and saved", alive.len()),
    );

    alive
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
