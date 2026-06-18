use std::collections::HashMap;
use std::sync::LazyLock;

/// Тестовые IP-адреса на loopback и соответствующие им страны.
///
/// При подключении к тестовому стенду (Docker) клиент использует эти IP,
/// и GeoIP резолвер возвращает нужную страну для проверки интерфейса.
#[allow(dead_code)]
const MOCK_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("127.0.0.2", "US"),
        ("127.0.0.3", "DE"),
        ("127.0.0.4", "JP"),
        ("127.0.0.5", "AU"),
    ])
});

/// Определяет страну сервера по его IP-адресу.
///
/// В тестовом режиме возвращает страну из `MOCK_MAP` для loopback-адресов.
/// В будущем здесь будет реальный GeoIP lookup через базу MaxMind GeoLite2.
#[allow(dead_code)]
pub fn resolve(ip: &str) -> String {
    MOCK_MAP
        .get(ip)
        .copied()
        .map(String::from)
        .unwrap_or_else(|| "Unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_mapping() {
        assert_eq!(resolve("127.0.0.2"), "US");
        assert_eq!(resolve("127.0.0.3"), "DE");
        assert_eq!(resolve("127.0.0.4"), "JP");
        assert_eq!(resolve("127.0.0.5"), "AU");
    }

    #[test]
    fn test_unknown_ip() {
        assert_eq!(resolve("1.2.3.4"), "Unknown");
        assert_eq!(resolve("127.0.0.1"), "Unknown");
        assert_eq!(resolve(""), "Unknown");
    }

    #[test]
    fn test_docker_network_ip() {
        // IP из Docker-сети — тоже Unknown (тестовая сеть)
        assert_eq!(resolve("172.17.0.2"), "Unknown");
    }
}
