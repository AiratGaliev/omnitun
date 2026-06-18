#![allow(dead_code)]

pub mod fetcher;
pub mod parser;

use crate::vpn::{ServerProfile, VpnError};
use fetcher::{HttpFetcher, SubscriptionFetcher};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionProfile {
    pub id: String,
    pub name: String,
    pub url: String,
    pub servers: Vec<ServerProfile>,
    pub updated_at: String,
}

/// Fetch subscription data from URL and parse into server profiles.
pub async fn fetch_subscription(url: &str) -> Result<Vec<ServerProfile>, VpnError> {
    let fetcher = HttpFetcher::new();
    let data = fetcher.fetch(url).await?;
    parser::parse_subscription(&data)
}
