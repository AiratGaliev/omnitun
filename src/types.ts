// FIXED:
//   - Added WireGuard fields (wgLocalAddress, wgAllowedIps, wgDns, wgMtu,
//     wgKeepalive) and `username` to the `Server` interface.
//   - These fields previously existed on the Rust `ServerProfile` struct but
//     were never propagated through the frontend types, so selecting a
//     WireGuard / SOCKS5-auth / HTTP-auth server from the dashboard caused
//     `start_proxy` to be called with `null` for all of them — and sing-box
//     then failed to authenticate or build a valid WireGuard outbound.

export type Tab = 'home' | 'profiles' | 'settings' | 'logs' | 'about';

export type ConnectionStatus =
  | 'disconnected'
  | 'connecting'
  | 'connected'
  | 'disconnecting'
  | 'error';

export type LogSource =
  | 'system'
  | 'proxy'
  | 'installer'
  | 'process'
  | 'subscription'
  | 'settings'
  | 'cache';

export interface LogEntry {
  id: string;
  timestamp: number;
  message: string;
  level: 'info' | 'error' | 'warning' | 'success';
  source: LogSource;
}

export interface Server {
  id: string;
  name: string;
  flag?: string;
  countryName?: string;
  cityName?: string;
  ping?: number;
  protocol?: string;
  server?: string;
  port?: number;
  uuid?: string;
  password?: string;
  method?: string;
  sni?: string;
  flow?: string;
  publicKey?: string;
  privateKey?: string;
  transportType?: string;
  transportPath?: string;
  transportHost?: string;
  realityPbk?: string;
  realitySid?: string;
  // ── WireGuard-specific fields (must be propagated end-to-end for
  //    WireGuard servers to connect). ───────────────────────────────────
  wgLocalAddress?: string;
  wgAllowedIps?: string;
  wgDns?: string;
  wgMtu?: number;
  wgKeepalive?: number;
  // ── Username for SOCKS5 / HTTP proxies that use username_password auth.
  //    Without this, sing-box receives `password` without `username` and
  //    the proxy connection fails to authenticate. ──────────────────────
  username?: string;
  behindCdn?: boolean;
  // ── Exit IP geoip (populated when outlet IP differs from entry IP) ─
  exitFlag?: string;
  exitCountryName?: string;
  exitCityName?: string;
  exitServer?: string;
}

export interface Profile {
  id: string;
  name: string;
  url: string;
  lastUpdated?: number;
  servers: Server[];
  error?: string;
  trafficUsed?: number;
  trafficTotal?: number;
  trafficExpiry?: string;
}

export interface ConnectionInfo {
  id: string;
  host?: string;
  metadata?: {
    network?: string;
    type?: string;
    source_ip?: string;
    destination_ip?: string;
    destination_port?: string;
    process?: string;
    process_path?: string;
    host?: string;
  };
  rule?: string;
  chains?: string[];
  upload_total?: number;
  download_total?: number;
  start?: string;
}

export interface SpeedTestResult {
  delay?: number;
  mean_delay?: number;
}

export interface CustomCountry {
  code: string;
  name: string;
  flag: string;
}

export interface Settings {
  acceptTerms: boolean;
  routingMode: 'system-proxy' | 'local-proxy';
  proxyPort: number;
  bypassRu: boolean;
  startWithWindows: boolean;
  autoConnectFastest: boolean;
  killSwitch: boolean;
  allowLan: boolean;
  customDns: boolean;
  dnsAddress: string;
  allowedCountries: string[];
  blockedCountries: string[];
  customCountries: CustomCountry[];
  allowedProtocols: string[];
  blockedProtocols: string[];
  language: 'en' | 'ru';
  autoUpdateInterval: number;
  antiDpi: boolean;
  enableMux: boolean;
  minimizeToTray: boolean;
  blockCdnNodes: boolean;
  ipinfoToken: string;
  tlsInsecure: boolean;
}

export interface ExitIpInfo {
  server: string;
  flag: string;
  country_name: string;
  city_name: string;
  /** The sing-box outbound tag that handled the request (e.g. "proxy-5"). */
  proxy_tag: string;
}

export interface RealIpInfo {
  server: string;
  flag: string;
  country_name: string;
  city_name: string;
}

/** Batch refresh progress event payload. */
export interface RefreshProgress {
  profile_id: string;
  total: number;
  tested: number;
  cache_hits: number;
  successful: number;
  failed: number;
  phase: string;
}

// ─── Rust backend structured errors ─────────────────────────────────────────
//
// ProxyError on the Rust side serializes to { code, message } via custom
// Serialize impl (see src-tauri/src/proxy/mod.rs).
//
// `code` is a camelCase string produced by #[serde(rename_all = "camelCase")]
// on the Rust ErrorCode enum. It matches an i18n key in en.json / ru.json
// directly — the frontend calls t(err.code, { defaultValue: err.message })
// without any mapping table.
//
// FORWARD COMPATIBILITY:
//   `code` is typed as `string` (not a union of known codes) so the frontend
//   gracefully handles codes from a newer backend. If the i18n key is missing,
//   t() falls back to err.message (English dev text from Rust).

export interface RustError {
  readonly code: string;
  readonly message: string;
}

/**
 * Type guard: checks if a value is a structured Rust error.
 * Returns false for:
 *   - strings (legacy commands returning Result<_, String>)
 *   - Error instances (JS errors)
 *   - null/undefined
 *   - objects missing `code` or `message`
 */
export function isRustError(err: unknown): err is RustError {
  return (
    typeof err === 'object' &&
    err !== null &&
    'code' in err &&
    'message' in err &&
    typeof (err as { code: unknown }).code === 'string' &&
    typeof (err as { message: unknown }).message === 'string'
  );
}
