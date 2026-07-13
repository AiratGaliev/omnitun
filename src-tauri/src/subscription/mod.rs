#![allow(dead_code)]

pub mod fetcher;
pub mod parser;

use crate::proxy::{ProxyError, ServerProfile};
use fetcher::{HttpFetcher, SubscriptionFetcher};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionProfile {
    pub id: String,
    pub name: String,
    pub url: String,
    pub servers: Vec<ServerProfile>,
    pub updated_at: String,
    /// When the servers were last batch-tested (without re-fetching
    /// the subscription URL). Separate from `updated_at` (subscription refresh).
    /// Enables two-tier cadence: refresh at auto_update_interval, test at auto_test_interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tested: Option<String>,
    /// Traffic info from Subscription-Userinfo header (HEAD request)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_expiry: Option<String>,
}

/// Fetch subscription data from URL and parse into server profiles.
/// If `proxy_port` is Some, routes the HTTP request through that local proxy.
pub async fn fetch_subscription(
    url: &str,
    proxy_port: Option<u16>,
) -> Result<Vec<ServerProfile>, ProxyError> {
    // Collect all URLs to try: direct + mirrors for raw.githubusercontent.com
    let urls: Vec<String> = if url.starts_with("https://raw.githubusercontent.com/") {
        let mirrors = [
            "https://ghproxy.net/https://raw.githubusercontent.com/",
            "https://gh-proxy.com/https://raw.githubusercontent.com/",
            "https://raw.githack.com/",
        ];
        let mut all = vec![url.to_string()];
        for prefix in &mirrors {
            all.push(url.replacen("https://raw.githubusercontent.com/", prefix, 1));
        }
        all
    } else {
        vec![url.to_string()]
    };

    // Race all URLs concurrently — return first success
    let mut set = tokio::task::JoinSet::new();
    for u in &urls {
        let url = u.clone();
        set.spawn(async move {
            let fetcher = HttpFetcher::new(proxy_port);
            let result = fetcher.fetch(&url).await;
            (url, result)
        });
    }

    let mut last_err = None;
    while let Some(res) = set.join_next().await {
        match res {
            Ok((_url, Ok(data))) => {
                // Cancel remaining tasks — we have a winner
                set.shutdown().await;
                return parser::parse_subscription(&data);
            }
            Ok((url, Err(e))) => {
                tracing::warn!(target: "subscription", "URL {url} failed: {e}");
                last_err = Some(e);
            }
            Err(join_err) => {
                tracing::warn!(target: "subscription", "Fetch task panicked: {join_err}");
            }
        }
    }

    Err(last_err.unwrap_or_else(|| ProxyError::subscription(
        crate::proxy::ErrorCode::SubscriptionFetchFailed,
        "All fetch URLs failed",
    )))
}

/// Fetch subscription traffic usage via HEAD request.
/// Reads Subscription-Userinfo header: `upload=0; download=12345; total=1073741824; expire=1700000000`
pub async fn fetch_subscription_traffic(
    url: &str,
    proxy_port: Option<u16>,
) -> Option<(u64, u64, Option<String>)> {
    // Build a client with no auto-redirect to avoid HEAD being converted to GET
    let client = reqwest::Client::builder()
        .user_agent("omnitun/0.1.0")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    // Proxy if sing-box is running
    let client = if let Some(port) = proxy_port {
        let proxy_url = format!("http://127.0.0.1:{port}");
        if let Ok(proxy) = reqwest::Proxy::http(&proxy_url) {
            reqwest::Client::builder()
                .user_agent("omnitun/0.1.0")
                .timeout(std::time::Duration::from_secs(10))
                .proxy(proxy)
                .build()
                .ok()?
        } else {
            client
        }
    } else {
        client
    };

    // Send HEAD request
    let response = client.head(url).send().await.ok()?;
    let headers = response.headers();

    // Parse Subscription-Userinfo header
    // Format: "upload=0; download=123456; total=1073741824; expire=1700000000"
    if let Some(userinfo) = headers.get("Subscription-Userinfo") {
        if let Ok(info_str) = userinfo.to_str() {
            let mut upload = 0u64;
            let mut download = 0u64;
            let mut total = 0u64;
            let mut expire: Option<String> = None;

            for part in info_str.split(';') {
                let part = part.trim();
                if let Some((key, value)) = part.split_once('=') {
                    let value = value.trim();
                    match key.trim() {
                        "upload" => upload = value.parse().unwrap_or(0),
                        "download" => download = value.parse().unwrap_or(0),
                        "total" => total = value.parse().unwrap_or(0),
                        "expire" => {
                            if let Ok(ts) = value.parse::<i64>() {
                                if ts > 0 {
                                    if let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) {
                                        expire = Some(dt.format("%Y-%m-%d").to_string());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            let used = upload + download;
            if total > 0 {
                return Some((used, total, expire));
            }
        }
    }
    None
}
