use crate::vpn::VpnError;

/// Trait so the fetcher can be mocked in tests.
#[async_trait::async_trait]
pub trait SubscriptionFetcher {
    async fn fetch(&self, url: &str) -> Result<String, VpnError>;
}

/// Production implementation using reqwest.
pub struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    pub fn new(proxy_port: Option<u16>) -> Self {
        let mut builder = reqwest::Client::builder()
            .user_agent("omnitun/0.1.0")
            .timeout(std::time::Duration::from_secs(15));

        // If sing-box is running (proxy_port from app settings), route fetch through it
        if let Some(port) = proxy_port {
            let proxy_url = format!("http://127.0.0.1:{port}");
            if let Ok(proxy) = reqwest::Proxy::http(&proxy_url) {
                builder = builder.proxy(proxy);
            }
        }

        let client = builder.build().unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for HttpFetcher {
    fn default() -> Self {
        Self::new(None)
    }
}

#[async_trait::async_trait]
impl SubscriptionFetcher for HttpFetcher {
    async fn fetch(&self, url: &str) -> Result<String, VpnError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| VpnError::Subscription(format!("HTTP request failed: {e}")))?;

        let body = response
            .text()
            .await
            .map_err(|e| VpnError::Subscription(format!("Failed to read response: {e}")))?;

        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockFetcher;

    #[async_trait::async_trait]
    impl SubscriptionFetcher for MockFetcher {
        async fn fetch(&self, _url: &str) -> Result<String, VpnError> {
            Ok(
                "vless://b831381d-6324-4d53-ad4f-8cda48b30811@127.0.0.2:8443?remark=Test"
                    .into(),
            )
        }
    }

    #[tokio::test]
    async fn test_mock_fetcher_returns_data() {
        let fetcher = MockFetcher;
        let data = fetcher.fetch("https://example.com/sub").await.unwrap();
        assert!(data.starts_with("vless://"));
    }

    #[tokio::test]
    async fn test_mock_fetcher_data_is_parseable() {
        let fetcher = MockFetcher;
        let data = fetcher.fetch("https://example.com/sub").await.unwrap();
        let servers = crate::subscription::parser::parse_subscription(&data).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].protocol, "vless");
    }

    #[tokio::test]
    async fn test_http_fetcher_invalid_url() {
        let fetcher = HttpFetcher::new();
        let result = fetcher.fetch("htp://[invalid-url").await;
        assert!(result.is_err());
    }
}
