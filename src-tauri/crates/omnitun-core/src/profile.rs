use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerProfile {
    pub id: String,
    pub protocol: String,
    pub server: String,
    pub port: u16,
    pub uuid: Option<String>,
    pub password: Option<String>,
    pub method: Option<String>,
    pub sni: Option<String>,
    pub flow: Option<String>,
    pub public_key: Option<String>,
    pub private_key: Option<String>,
    // Transport and Reality for modern VLESS/Trojan
    pub transport_type: Option<String>,
    pub transport_path: Option<String>,
    pub transport_host: Option<String>,
    pub reality_pbk: Option<String>,
    pub reality_sid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ping_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city_name: Option<String>,
    // WireGuard-specific fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_local_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_allowed_ips: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_dns: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_mtu: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_keepalive: Option<u16>,
    pub username: Option<String>,
    /// Original subscription-provided name (for display fallback). Not serialized.
    #[serde(skip)]
    pub original_name: String,
    /// Whether this server routes through a CDN (unstable outlet IP).
    /// CDN-routed servers have unstable outlet IPs (different per request).
    #[serde(default, skip_serializing_if = "is_false")]
    pub behind_cdn: bool,
    /// Exit IP geoip — last known exit IP address when connected through this server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_country_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_city_name: Option<String>,
}

/// Serde helper: skip serializing `behind_cdn` when false.
fn is_false(v: &bool) -> bool {
    !v
}

// ─── CDN detection ─────────────────────────────────────────────────────────

/// Keyword-based CDN detection — checks if the domain contains known CDN
/// provider names or CDN-related patterns.
///
/// This catches servers routed through Cloudflare/Fastly/CloudFront CDNs
/// (like `cdn.gamingdll.ir`, `global.ssl.fastly.net`) where the transport_host
/// or sni indicates CDN routing. These servers typically have unstable outlet IPs.
///
/// In addition to keywords, the ASN database (GeoLite2-ASN.mmdb) is checked
/// during batch latency testing for servers whose outlet IP IS reachable
/// through the proxy (covers Cloudflare Workers/WARP where ASN = 13335 etc.).
fn keyword_cdn_check(host: &str) -> bool {
    let lower = host.to_lowercase();
    lower.contains("fastly")
        || lower.contains("cloudflare")
        || lower.contains("cloudfront")
        || lower.contains("akamai")
        || lower.contains("cdn.")
        || lower.contains(".worker.")
        || lower.starts_with("worker.")
        || lower.ends_with(".workers.dev")
        || lower.ends_with(".pages.dev")
        || lower.ends_with(".cfargotunnel.com")
}

/// Lightweight initial CDN detection via keyword check on transport_host, sni.
///
/// The real detection happens during batch testing via GeoLite2-ASN.mmdb
/// (`geoip::is_cdn_entry_ip()`), which examines the actual entry IP.
/// This is a fast pre-check for obviously serverless domains, so the UI can show
/// the CDN badge even before batch testing runs.
///
/// DNS CIDR resolution on entry domains is NOT used here because a VPS behind
/// Cloudflare CDN has a stable outlet IP and is NOT a CDN node in the
/// problematic sense. Only Workers/WARP/Pages have unstable outlet IPs.
pub fn is_cdn_server(s: &ServerProfile) -> bool {
    if let Some(ref host) = s.transport_host {
        if keyword_cdn_check(host) {
            return true;
        }
    }
    if let Some(ref sni) = s.sni {
        if keyword_cdn_check(sni) {
            return true;
        }
    }
    if keyword_cdn_check(&s.server) {
        return true;
    }
    false
}

// ─── ServerProfile impl ────────────────────────────────────────────────────

impl ServerProfile {
    /// Construct a human-readable display name from GeoIP data or fallback to original_name.
    /// NOTE: city is intentionally excluded per user requirement.
    pub fn display_name(&self) -> String {
        if let Some(country) = &self.country_name {
            country.clone()
        } else if !self.original_name.is_empty() {
            self.original_name.clone()
        } else {
            format!("{}:{}", self.server, self.port)
        }
    }

    /// Returns `true` if this server uses a UDP-based transport protocol
    /// (hysteria2, tuic, wireguard).
    ///
    /// UDP protocols do NOT listen on TCP — a `TcpStream::connect` to their
    /// port will always fail (connection refused / timeout). This means the
    /// TCP reachability ping in the preflight phase must be skipped for
    /// these protocols, otherwise they are filtered out as "unreachable" and
    /// never reach the batch test stage — so the user never sees them in
    /// their profile.
    ///
    /// The batch test (`test_single_server`) already handles UDP gracefully:
    /// if the SOCKS5 HTTP latency test fails (UDP can't carry raw TCP through
    /// a plain SOCKS5 proxy in some configs), the server is still added with
    /// `ping_ms = 0`. Sing-box itself tunnels TCP-over-UDP for these
    /// outbounds, so the real proxy connection works fine.
    ///
    /// This helper is the single source of truth for "is this protocol
    /// UDP-based" — used by both the preflight phase (TCP ping skip) and
    /// `test_single_server` (latency test degradation).
    pub fn is_udp_protocol(&self) -> bool {
        matches!(self.protocol.as_str(), "hysteria2" | "tuic" | "wireguard")
    }

    /// ═══════════════════════════════════════════════════════════════════
    /// INVARIANT CHECKER: I-CDN-1 (CDN servers must NOT have exit data)
    /// ═══════════════════════════════════════════════════════════════════
    ///
    /// ## What this checks
    ///
    /// If `behind_cdn == true`, all of `exit_server`, `exit_flag`,
    /// `exit_country_name`, `exit_city_name` MUST be `None`. If any are
    /// `Some`, this method logs `tracing::error!` with `target:
    /// "cdn-invariant"` and a `[BUG][I-CDN-1]` prefix.
    ///
    /// This method does **not** modify the server — it only observes.
    /// Clearing exit data is the caller's responsibility.
    ///
    /// ## Regression history
    ///
    /// This checker was added after several regressions where CDN servers
    /// accumulated exit data despite the invariant. All three incidents
    /// below would have been caught immediately by this checker:
    ///
    /// 1. **2026-07-01 — exit restore loop in refresh**:
    ///    `process_servers_incremental` cleared CDN exit data, but `ProfileManager::refresh`
    ///    restored them from `old_exit` without checking `behind_cdn`.
    ///
    /// 2. **2026-07-03 — test_single_server overrode behind_cdn**:
    ///    A non-CDN server whose outlet IP happened to fall in CDN ranges
    ///    (Cloudflare WARP, Scaleway, Oracle Cloud) would get `behind_cdn=true`
    ///    and lose its outlet IP.
    ///
    /// 3. **2026-07-04 — start_proxy emitted empty exit-ip-changed**:
    ///    A non-CDN server with CDN-range outlet IP got an empty `ExitIpInfo`
    ///    emitted to the frontend, losing the exit IP.
    ///
    /// **Without this checker** any future regression of this kind would be
    /// invisible — the user would see "no exit IP" with no explanation,
    /// and the bug could live unnoticed for weeks.
    ///
    /// ## Why `error!` level (DO NOT DOWNGRADE)
    ///
    /// 🚫 Do **NOT** downgrade to `warn!` or `info!`.
    /// 🚫 Do **NOT** remove the logging "because the caller cleans up anyway".
    /// 🚫 Do **NOT** "simplify" by removing the regression history.
    ///
    /// This is an **invariant violation**, not a warning. If `behind_cdn=true`
    /// coexists with exit data, either:
    ///   (a) CDN classification is wrong (server is not actually CDN), or
    ///   (b) Exit data is stale/fabricated (server is CDN but someone wrote
    ///       exit data — a pipeline bug).
    /// Either case corrupts country-based filtering for the user.
    ///
    /// Clearing exit data is **defence in depth**. Logging is the **only**
    /// signal that the invariant was violated. Without it we cannot detect
    /// regressions.
    ///
    /// ## Why not `panic!`
    ///
    /// A panic would crash the app over a data-consistency issue that is
    /// worse than logging and continuing. The caller is responsible for
    /// clearing the data after this method returns.
    ///
    /// ## Usage
    ///
    /// Call **before** clearing exit data on a CDN server:
    /// ```ignore
    /// if server.behind_cdn {
    ///     server.assert_no_cdn_exit_data("context_name");
    ///     server.exit_server = None;
    ///     // ... clear other fields ...
    /// }
    /// ```
    ///
    /// Also call in periodic background checker, on profile load,
    /// and when starting a proxy to a CDN server.
    pub fn assert_no_cdn_exit_data(&self, context: &str) {
        if !self.behind_cdn {
            return;
        }
        let violations: Vec<&str> = [
            ("exit_server", self.exit_server.is_some()),
            ("exit_flag", self.exit_flag.is_some()),
            ("exit_country_name", self.exit_country_name.is_some()),
            ("exit_city_name", self.exit_city_name.is_some()),
        ]
        .iter()
        .filter(|(_, has)| *has)
        .map(|(name, _)| *name)
        .collect();
        if violations.is_empty() {
            return;
        }
        tracing::error!(
            target: "cdn-invariant",
            "[BUG][I-CDN-1] INVARIANT VIOLATION in {}: \
             CDN server {} ({}:{}, name={:?}) has exit data despite behind_cdn=true. \
             Violating fields: [{}]. \
             Values: exit_server={:?} exit_flag={:?} exit_country={:?} exit_city={:?}. \
             This is a REGRESSION — see assert_no_cdn_exit_data doc-comment for history. \
             The caller will clear these fields, but the underlying bug must be fixed.",
            context,
            self.id,
            self.server,
            self.port,
            self.original_name,
            violations.join(", "),
            self.exit_server,
            self.exit_flag,
            self.exit_country_name,
            self.exit_city_name,
        );
    }
}
