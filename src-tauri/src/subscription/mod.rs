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
/// If `proxy_port` is Some, routes the HTTP request through that local proxy.
pub async fn fetch_subscription(url: &str, proxy_port: Option<u16>) -> Result<Vec<ServerProfile>, VpnError> {
    let fetcher = HttpFetcher::new(proxy_port);
    let data = fetcher.fetch(url).await?;
    parser::parse_subscription(&data)
}
