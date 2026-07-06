use crate::core::Runtime;
use crate::proxy::ProxyError;
use crate::subscription::SubscriptionProfile;

/// Core profile manager — delegates to ProfileManager with Runtime abstraction.
pub struct CoreProfileManager;

impl CoreProfileManager {
    /// Add a subscription URL.
    pub async fn add_subscription(
        rt: &dyn Runtime,
        url: String,
        name: String,
    ) -> Result<SubscriptionProfile, ProxyError> {
        let profiles = rt.profile_state();
        let mut guard = profiles.lock().await;

        if guard.list().iter().any(|p| p.url == url) {
            return Err(ProxyError::subscription(
                crate::proxy::ErrorCode::SubscriptionDuplicateUrl,
                format!("Subscription with URL '{url}' already exists"),
            ));
        }

        let profile = SubscriptionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            url,
            servers: Vec::new(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            traffic_used: None,
            traffic_total: None,
            traffic_expiry: None,
        };

        guard.list_mut().push(profile.clone());
        guard.save()?;
        Ok(profile)
    }

    /// Remove a subscription by ID.
    pub async fn remove_subscription(
        rt: &dyn Runtime,
        id: String,
    ) -> Result<(), ProxyError> {
        let profiles = rt.profile_state();
        let mut guard = profiles.lock().await;

        let pos = guard.list().iter().position(|p| p.id == id)
            .ok_or_else(|| ProxyError::subscription(
                crate::proxy::ErrorCode::SubscriptionNotFound,
                format!("Profile '{id}' not found"),
            ))?;
        guard.list_mut().remove(pos);
        guard.save()?;
        Ok(())
    }

    /// List all subscriptions (async — returns a snapshot).
    pub async fn list_subscriptions(rt: &dyn Runtime) -> Vec<SubscriptionProfile> {
        let profiles = rt.profile_state();
        let guard = profiles.lock().await;
        guard.list().to_vec()
    }
}
