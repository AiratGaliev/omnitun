// FIXED (root cause of "app does not connect to specific server from the
// dashboard list"):
//
// 1. `RustServerProfile` interface was missing WireGuard fields
//    (wg_local_address, wg_allowed_ips, wg_dns, wg_mtu, wg_keepalive)
//    and `username`. The Rust `ServerProfile` struct HAS these fields, but
//    the frontend TS interface didn't declare them, so `mapRustProfile`
//    silently dropped them when servers were loaded into the dashboard.
//
// 2. `mapRustProfile` was not mapping those fields onto the frontend
//    `Server` object. As a result, when the user clicked on a WireGuard /
//    SOCKS5-auth / HTTP-auth server and pressed Connect, the active server
//    had no WireGuard or username data attached.
//
// 3. `toggleConnection` was hardcoding `wg_local_address: null`,
//    `wg_allowed_ips: null`, `wg_dns: null`, `wg_mtu: null`,
//    `wg_keepalive: null`, `username: null` when sending the profile to
//    the Rust `start_proxy` command. Combined with bugs #1 and #2, this
//    guaranteed that those servers could never be connected to from the
//    dashboard. Now those fields are sourced from `activeServer`.
//
// 4. `alternativeServers` was always an empty array, which defeated the
//    `autoConnectFastest` setting — the Rust backend then never built a
//    `urltest` group for failover. When `autoConnectFastest` is enabled,
//    we now pass the remaining known servers (excluding the selected one)
//    as alternative servers so the backend can construct the urltest group
//    and auto-pick the fastest outbound.

import {create} from 'zustand';
import {invoke} from '@tauri-apps/api/core';
import {listen} from '@tauri-apps/api/event';
import {toast} from 'sonner';
import {i18n} from './i18n';
import type {
  ConnectionInfo,
  ConnectionStatus,
  ExitIpInfo,
  LogEntry,
  LogSource,
  Profile,
  RefreshProgress,
  Server,
  Settings,
  SpeedTestResult,
} from './types';

interface StatusPayload {
  status: string | { error: string };
}

interface TrafficPayload {
  up: number;
  down: number;
}

interface RustServerProfile {
  id: string;
  protocol: string;
  server: string;
  port: number;
  uuid?: string;
  password?: string;
  method?: string;
  sni?: string;
  flow?: string;
  public_key?: string;
  private_key?: string;
  transport_type?: string;
  transport_path?: string;
  transport_host?: string;
  reality_pbk?: string;
  reality_sid?: string;
  ping_ms?: number;
  flag?: string;
  country_name?: string;
  city_name?: string;
  // ── Fields that were previously missing from this interface ───────────
  // Rust's `ServerProfile` struct (see src-tauri/src/proxy/mod.rs) defines
  // all of these, but the old TS interface omitted them, so they were
  // dropped during `mapRustProfile` and never reached the dashboard.
  wg_local_address?: string;
  wg_allowed_ips?: string;
  wg_dns?: string;
  wg_mtu?: number;
  wg_keepalive?: number;
  username?: string;
  // NOTE: `original_name` has `#[serde(skip)]` on the Rust side, so it is
  // NOT present in the JSON. The display name is derived from
  // country_name / city_name in `mapRustProfile`.
  behind_cdn?: boolean;
  // ── Exit IP geoip fields ──────────────────────────────────────────
  exit_flag?: string;
  exit_country_name?: string;
  exit_city_name?: string;
  exit_server?: string;
}

export interface RustSubscriptionProfile {
  id: string;
  name: string;
  url: string;
  servers: RustServerProfile[];
  updated_at: string;
  traffic_used?: number;
  traffic_total?: number;
  traffic_expiry?: string;
}

interface ProxyStore {
  status: ConnectionStatus;
  activeServer: Server | null;
  servers: Server[];
  profiles: Profile[];
  settings: Settings;
  hasOnboarded: boolean;
  hasAcceptedTerms: boolean;
  speeds: { up: number; down: number };
  connectedAt: number | null;
  logs: LogEntry[];
  settingsVersion: number;
  /** Per-profile batch refresh progress (profile_id → progress or null). */
  refreshProgress: Record<string, RefreshProgress | null>;

  setStatus: (status: ConnectionStatus) => void;
  setActiveServer: (server: Server) => void;
  setServers: (servers: Server[]) => void;
  setConnectedAt: (timestamp: number | null) => void;
  addProfile: (data: { name: string; url: string }) => Promise<void>;
  updateProfile: (id: string, data: Partial<Profile>) => void;
  removeProfile: (id: string) => Promise<void>;
  refreshProfile: (id: string) => Promise<void>;
  loadProfiles: () => Promise<void>;
  updateSettings: (settings: Partial<Settings>) => void;
  saveSettings: () => Promise<void>;
  loadSettings: () => Promise<void>;
  addLog: (message: string, level?: LogEntry['level'], source?: LogSource) => void;
  clearLogs: () => void;
  completeOnboarding: () => void;
  acceptTerms: () => void;
  setSpeeds: (speeds: { up: number; down: number }) => void;
  toggleConnection: () => Promise<void>;
  initListener: () => Promise<() => void>;
  exitIp: ExitIpInfo | null;
  fetchExitIp: () => Promise<void>;
  realIp: string | null;
  fetchRealIp: () => Promise<void>;
  speedTestResult: SpeedTestResult | null;
  runSpeedTest: () => Promise<SpeedTestResult | null>;
  connections: ConnectionInfo[];
  refreshConnections: () => Promise<void>;
  _healthCheckTimer: ReturnType<typeof setInterval> | null;
  _isReconnecting: boolean;
  /** Toggle guard — prevents concurrent connect/disconnect. */
  _isConnecting: boolean;
  /** Set of profile IDs currently being refreshed (dedup guard). */
  _refreshingIds: string[];
  /** Servers active in the current proxy session (ordered: primary then alternatives). */
  _sessionServers: Server[];
}

const defaultServers: Server[] = [];

function mapConnectionStatus(payload: StatusPayload): ConnectionStatus {
  if (typeof payload.status === 'string') {
    return payload.status as ConnectionStatus;
  }
  return 'error';
}

function extractError(payload: StatusPayload): string | undefined {
  if (typeof payload.status === 'object' && 'error' in payload.status) {
    return payload.status.error;
  }
  return undefined;
}

export function mapRustProfile(rust: RustSubscriptionProfile): Profile {
  return {
    id: rust.id,
    name: rust.name,
    url: rust.url,
    lastUpdated: Date.parse(rust.updated_at) || Date.now(),
    trafficUsed: rust.traffic_used,
    trafficTotal: rust.traffic_total,
    trafficExpiry: rust.traffic_expiry,
    servers: rust.servers.map((s: RustServerProfile): Server => {
      const cn = s.country_name;
      const displayName =
        cn && s.city_name ? `${cn}, ${s.city_name}` : (cn ?? `${s.server}:${String(s.port)}`);
      return {
        id: s.id,
        name: displayName,
        protocol: s.protocol,
        server: s.server,
        port: s.port,
        uuid: s.uuid,
        password: s.password,
        method: s.method,
        sni: s.sni,
        flow: s.flow,
        publicKey: s.public_key,
        privateKey: s.private_key,
        transportType: s.transport_type,
        transportPath: s.transport_path,
        transportHost: s.transport_host,
        realityPbk: s.reality_pbk,
        realitySid: s.reality_sid,
        ping: s.ping_ms,
        flag: s.flag ?? undefined,
        countryName: s.country_name ?? undefined,
        cityName: s.city_name ?? undefined,
        exitFlag: s.exit_flag ?? undefined,
        exitCountryName: s.exit_country_name ?? undefined,
        exitCityName: s.exit_city_name ?? undefined,
        exitServer: s.exit_server ?? undefined,
        // ── FIX: propagate WireGuard + username fields so the dashboard
        //    can show them and `toggleConnection` can forward them to
        //    `start_proxy`. Without this, WireGuard / SOCKS5-auth /
        //    HTTP-auth servers silently lose their connection data
        //    when the user picks them in the dropdown. ─────────────────
        wgLocalAddress: s.wg_local_address ?? undefined,
        wgAllowedIps: s.wg_allowed_ips ?? undefined,
        wgDns: s.wg_dns ?? undefined,
        wgMtu: s.wg_mtu ?? undefined,
        wgKeepalive: s.wg_keepalive ?? undefined,
        username: s.username ?? undefined,
        behindCdn: s.behind_cdn ?? undefined,
      };
    }),
  };
}

/**
 * Convert a frontend `Server` object into the snake_case payload shape that
 * the Rust `start_proxy` command expects (matches `ServerProfile` in
 * src-tauri/src/proxy/mod.rs).
 *
 * IMPORTANT: every field present on the Rust struct is included here,
 * including the WireGuard fields and `username`. Previously these were
 * hardcoded to `null`, which is the root cause of the "cannot connect to
 * specific server" bug reported in the dashboard.
 */
function toRustServerProfile(s: Server | null, fallbackName: string): Record<string, unknown> {
  return {
    id: s?.id ?? 'default',
    name: s?.name ?? fallbackName,
    protocol: s?.protocol ?? 'vless',
    server: s?.server ?? '127.0.0.1',
    port: s?.port ?? 0,
    uuid: s?.uuid ?? null,
    password: s?.password ?? null,
    method: s?.method ?? null,
    sni: s?.sni ?? null,
    flow: s?.flow ?? null,
    public_key: s?.publicKey ?? null,
    private_key: s?.privateKey ?? null,
    transport_type: s?.transportType ?? null,
    transport_path: s?.transportPath ?? null,
    transport_host: s?.transportHost ?? null,
    reality_pbk: s?.realityPbk ?? null,
    reality_sid: s?.realitySid ?? null,
    ping_ms: null,
    flag: s?.flag ?? null,
    // ── FIX: source these from the active server instead of hardcoding
    //    `null`. WireGuard / SOCKS5-auth / HTTP-auth servers REQUIRE these
    //    values to produce a valid sing-box outbound. ────────────────────
    wg_local_address: s?.wgLocalAddress ?? null,
    wg_allowed_ips: s?.wgAllowedIps ?? null,
    wg_dns: s?.wgDns ?? null,
    wg_mtu: s?.wgMtu ?? null,
    wg_keepalive: s?.wgKeepalive ?? null,
    username: s?.username ?? null,
    // NOTE: behind_cdn intentionally NOT sent from frontend — Rust determines
    // CDN status from the profile manager's enrich_loaded (GeoIP/CIDR lookup).
    // Sending it from the frontend would allow stale/missing data to bypass
    // the real CDN check in start_proxy.
  };
}

export const useAppStore = create<ProxyStore>((set, get) => ({
  status: 'disconnected',
  activeServer: null,
  servers: defaultServers,
  profiles: [],
  refreshProgress: {},
  hasOnboarded: false,
  hasAcceptedTerms: false,
  speeds: { up: 0, down: 0 },
  connectedAt: null,
  logs: [],
  settingsVersion: 0,
  exitIp: null,
  realIp: null,
  speedTestResult: null,
  connections: [],
  _healthCheckTimer: null,
  _isReconnecting: false,
  _isConnecting: false,
  _refreshingIds: [],
  _sessionServers: [],
  settings: {
    acceptTerms: false,
    routingMode: 'system-proxy',
    proxyPort: 1080,
    bypassRu: true,
    startWithWindows: true,
    autoConnectFastest: true,
    killSwitch: false,
    allowLan: true,
    customDns: false,
    dnsAddress: '1.1.1.1',
    allowedCountries: [],
    blockedCountries: [],
    customCountries: [],
    allowedProtocols: [],
    blockedProtocols: [],
    language: 'en',
    autoUpdateInterval: 5,
    antiDpi: true,
    enableMux: false,
    minimizeToTray: false,
    blockCdnNodes: false,
    ipinfoToken: '',
  },

  setStatus: (status) => {
    set({ status });
  },
  setActiveServer: (server) => {
    const prev = get().activeServer;
    set({ activeServer: server });
    if (server.id !== prev?.id) {
      get().addLog(
        `[store] activeServer: ${prev?.name ?? 'null'} → ${server.name} (${server.id.slice(0, 8)})`,
        'info',
        'system',
      );
    }
  },
  setServers: (servers) => {
    const prev = get().servers;
    set((state) => {
      const currentActive = state.activeServer;
      const updatedActive = currentActive
        ? servers.find((s) => s.id === currentActive.id) ?? currentActive
        : null;
      return { servers, activeServer: updatedActive };
    });
    if (prev.length !== servers.length) {
      get().addLog(
        `[store] servers: ${String(prev.length)} → ${String(servers.length)}`,
        'info',
        'system',
      );
    }
  },
  setConnectedAt: (timestamp) => {
    set({ connectedAt: timestamp });
  },

  addProfile: async (data) => {
    try {
      const result = await invoke<RustSubscriptionProfile>('add_subscription', {
        name: data.name,
        url: data.url,
      });
      const profile = mapRustProfile(result);
      set((state) => {
        const allServers = [...state.servers, ...profile.servers];
        const autoSelect = state.settings.autoConnectFastest;
        const newActive = autoSelect
          ? allServers.reduce((best, s) => ((s.ping ?? 9999) < (best.ping ?? 9999) ? s : best))
          : (state.activeServer ?? allServers[0] ?? null);
        return {
          profiles: [...state.profiles, profile],
          servers: allServers,
          activeServer: newActive,
        };
      });
      get().addLog(
        `Profile "${data.name}" added (${String(profile.servers.length)} server(s))`,
        'success',
        'subscription',
      );
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      get().addLog(`Failed to add profile: ${msg}`, 'error', 'subscription');
      throw e;
    }
  },

  updateProfile: (id, data) => {
    set((state) => ({
      profiles: state.profiles.map((p) => (p.id === id ? { ...p, ...data } : p)),
    }));
  },

  removeProfile: async (id) => {
    try {
      await invoke('remove_subscription', { id });
      set((state) => {
        const removed = state.profiles.find((p) => p.id === id);
        const removedIds = new Set(removed?.servers.map((s) => s.id) ?? []);
        const remaining = state.servers.filter((s) => !removedIds.has(s.id));
        const wasActiveRemoved = removedIds.has(state.activeServer?.id ?? '');
        const autoSelect = state.settings.autoConnectFastest;
        return {
          profiles: state.profiles.filter((p) => p.id !== id),
          servers: remaining,
          activeServer: wasActiveRemoved
            ? autoSelect
              ? remaining.reduce(
                  (best, s) => ((s.ping ?? 9999) < (best.ping ?? 9999) ? s : best),
                  remaining[0],
                )
              : (remaining[0] ?? null)
            : state.activeServer,
        };
      });
      get().addLog('Profile removed', 'success', 'subscription');
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      get().addLog(`Failed to remove profile: ${msg}`, 'error', 'subscription');
      throw e;
    }
  },

  refreshProfile: async (id) => {
    // Guard: prevent duplicate concurrent refreshes for the same profile
    {
      const state = get();
      if (state._refreshingIds.includes(id)) {
        state.addLog(
          `[refresh] profile "${id.slice(0, 8)}" already refreshing — skipping`,
          'info',
          'system',
        );
        return;
      }
    }
    set((state) => ({ _refreshingIds: [...state._refreshingIds, id] }));
    try {
      const result = await invoke<RustSubscriptionProfile>('refresh_subscription', { id });
      const updated = mapRustProfile(result);
      set((state) => {
        const newProfiles = state.profiles.map((p) => (p.id === id ? { ...p, ...updated } : p));
        const allServers = newProfiles.flatMap((p) => p.servers);
        const activeServerStillExists = allServers.some((s) => s.id === state.activeServer?.id);
        // Auto-select fastest only when enabled. When disabled, never swap
        // even if the current server ID is gone — user picks manually.
        const autoSelect = state.settings.autoConnectFastest;
        const oldActiveName = state.activeServer?.name ?? state.activeServer?.server ?? 'none';
        const newActiveServer = autoSelect
          ? allServers.reduce((best, s) => ((s.ping ?? 9999) < (best.ping ?? 9999) ? s : best))
          : activeServerStillExists
            ? state.activeServer
            : (allServers[0] ?? null);
        if (state.activeServer && state.activeServer.id !== newActiveServer?.id) {
          const reason = autoSelect
            ? !activeServerStillExists
              ? 'ID not found after refresh'
              : 'autoConnectFastest'
            : 'current server was removed';
          const newName = newActiveServer?.name ?? newActiveServer?.server ?? 'none';
          get().addLog(
            `Active server changed: ${oldActiveName} → ${newName} (reason: ${reason})`,
            'warning',
            'system',
          );
        }
        return {
          profiles: newProfiles,
          servers: allServers,
          activeServer: newActiveServer,
        };
      });
      get().addLog(
        `Profile "${updated.name}" refreshed (${String(updated.servers.length)} server(s))`,
        'success',
        'subscription',
      );
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      // Network may be blocking GitHub raw — log once on first failure, skip spam
      get().addLog(`Failed to refresh profile: ${msg}`, 'error', 'subscription');
      // Reset lastUpdated so timer doesn't retry every ~55s.
      // Wait full autoUpdateInterval before next attempt.
      set((state) => ({
        profiles: state.profiles.map((p) =>
          p.id === id ? { ...p, lastUpdated: Date.now() + Math.random() * 60000 } : p,
        ),
      }));
      throw e;
    } finally {
      set((state) => ({
        _refreshingIds: state._refreshingIds.filter((rid) => rid !== id),
      }));
    }
  },

  loadProfiles: async () => {
    try {
      const results = await invoke<RustSubscriptionProfile[]>('get_subscriptions');
      const profiles = results.map(mapRustProfile);
      const allServers = profiles.flatMap((p) => p.servers);
      const state = get();
      const oldActive = state.activeServer;
      const autoSelect = state.settings.autoConnectFastest;
      // When autoConnectFastest is enabled, pick the fastest server so
      // the initial connection uses the best node. When disabled, keep
      // the existing selection (fall back to first available if removed).
      const newActive: Server | null = autoSelect
        ? allServers.length > 0
          ? allServers.reduce((best, s) => ((s.ping ?? 9999) < (best.ping ?? 9999) ? s : best))
          : null
        : (allServers.find((s) => s.id === oldActive?.id) ?? allServers[0] ?? null);
      set({
        profiles,
        servers: allServers,
        activeServer: newActive,
        // If profiles exist, onboarding is complete (flag not persisted)
        hasOnboarded: profiles.length > 0 || get().hasOnboarded,
      });
      if (newActive?.id !== oldActive?.id) {
        const reason = autoSelect ? 'fastest' : 'previous was removed';
        get().addLog(
          `[loadProfiles] activeServer changed: ${oldActive?.name ?? 'null'} → ${newActive?.name ?? 'null'} (${reason})`,
          'info',
          'system',
        );
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      get().addLog(`Failed to load profiles: ${msg}`, 'warning', 'subscription');
    }
  },

  updateSettings: (newSettings) => {
    set((state) => ({
      settings: { ...state.settings, ...newSettings },
      settingsVersion: state.settingsVersion + 1,
    }));
  },

  saveSettings: async () => {
    try {
      const { settings } = get();
      await invoke('update_settings', { newSettings: settings });
      // Re-load profiles so filters are applied server-side
      await get().loadProfiles();

      // If ipinfo token is non-empty, trigger ipinfo database download
      if (settings.ipinfoToken) {
        const toastId = toast.loading(i18n.t('ipinfoDownloading'));
        try {
          const result = await invoke<string>('download_ipinfo_db');
          if (result === 'downloaded') {
            toast.success(i18n.t('ipinfoDownloaded'), { id: toastId });
          } else {
            toast.info(i18n.t('ipinfoCached'), { id: toastId });
          }
        } catch (e) {
          toast.error(`${i18n.t('ipinfoDownloadFailed')}: ${String(e)}`, { id: toastId });
        }
      }
    } catch (e) {
      get().addLog(`Failed to save settings: ${String(e)}`, 'error', 'settings');
    }
  },

  loadSettings: async () => {
    try {
      const result = await invoke<Settings>('get_settings');
      // Migration: legacy 'proxy' mode → 'system-proxy'
      const rawMode = (result as { routingMode: string }).routingMode;
      if (rawMode === 'proxy') {
        (result as { routingMode: string }).routingMode = 'system-proxy';
      }
      set({ settings: result });
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      get().addLog(`Failed to load settings: ${msg}`, 'warning', 'settings');
    }
  },

  addLog: (message: string, level: LogEntry['level'] = 'info', source: LogSource = 'system') => {
    set((state) => ({
      logs: [
        {
          id: crypto.randomUUID(),
          timestamp: Date.now(),
          message,
          level,
          source,
        },
        ...state.logs,
      ].slice(0, 1000),
    }));
    // Duplicate ALL messages to the Rust JSONL file so every log entry
    // survives UI restart and can be correlated with Rust-side logs.
    invoke('log_frontend', { message, level, source }).catch(() => {});
  },

  clearLogs: () => {
    set({ logs: [] });
  },

  completeOnboarding: () => {
    set({ hasOnboarded: true });
    get().addLog('Onboarding completed', 'success', 'system');
  },
  acceptTerms: () => {
    set({ hasAcceptedTerms: true });
    get().addLog('Terms of service accepted', 'success', 'system');
  },
  setSpeeds: (speeds) => {
    set({ speeds });
  },

  fetchExitIp: async () => {
    try {
      const info = await invoke<ExitIpInfo>('outlet_ip');
      set({ exitIp: info });
    } catch (e) {
      get().addLog(`Failed to fetch exit IP: ${String(e)}`, 'warning', 'system');
      set({ exitIp: null });
    }
  },

  fetchRealIp: async () => {
    try {
      const ip = await invoke<string>('get_real_ip');
      set({ realIp: ip });
    } catch (e) {
      get().addLog(`Failed to fetch real IP: ${String(e)}`, 'warning', 'system');
      set({ realIp: null });
    }
  },

  runSpeedTest: async () => {
    try {
      const results = await invoke<SpeedTestResult[]>('speed_test');
      const best =
        results.length > 0
          ? results.reduce((a, b) => ((a.delay ?? 999) < (b.delay ?? 999) ? a : b))
          : null;
      set({ speedTestResult: best });
      return best;
    } catch (e) {
      get().addLog(`Speed test failed: ${String(e)}`, 'warning', 'system');
      set({ speedTestResult: null });
      return null;
    }
  },

  refreshConnections: async () => {
    try {
      const conns = await invoke<ConnectionInfo[]>('get_connections');
      set({ connections: conns });
    } catch (e) {
      // Not connected is expected — log debug level
      get().addLog(`refreshConnections skipped: ${String(e)}`, 'info', 'system');
    }
  },

  toggleConnection: async () => {
    const { status, activeServer, servers, addLog, settings, _isReconnecting, _isConnecting } = get();
    const logToggle = (msg: string, level: 'info' | 'warning' | 'error' | 'success' = 'info') => {
      addLog(`[toggle] ${msg}`, level, 'system');
    };
    logToggle(`called — status=${status} _isReconnecting=${String(_isReconnecting)} _isConnecting=${String(_isConnecting)} server=${activeServer?.name ?? 'null'}`);
    if (_isReconnecting || _isConnecting) {
      logToggle(`BLOCKED by guard (_isReconnecting=${String(_isReconnecting)} _isConnecting=${String(_isConnecting)})`, 'warning');
      return;
    }
    // Log the server we're connecting to
    const connectServerName = activeServer?.name ?? 'null';
    const connectServerId = activeServer?.id.slice(0, 8) ?? 'null';
    logToggle(
      `request — server: ${connectServerName} (${connectServerId}) proto=${activeServer?.protocol ?? '?'} addr=${activeServer?.server ?? '?'}:${String(activeServer?.port ?? '?')}`,
    );
    try {
      if (status === 'disconnected' || status === 'error') {
        logToggle('CONNECT path');
        set({ _isConnecting: true });
        addLog(`Connecting to ${activeServer?.name ?? 'Default'}...`, 'info', 'system');
        set({ status: 'connecting' });
        logToggle('status set to connecting, _isConnecting=true');
        // Persist settings before connecting (routingMode, proxyPort from Dashboard)
        await get()
          .saveSettings()
          .catch((e) => get().addLog('saveSettings failed: ' + String(e), 'warning', 'system'));
        logToggle('saveSettings done');

        // FIX: build the rust-side profile from `activeServer` WITHOUT
        // dropping WireGuard / username fields. Previously all of these
        // were hardcoded to `null`, which is why WireGuard / SOCKS5-auth
        // / HTTP-auth servers couldn't be connected to from the dashboard.
        const profile = toRustServerProfile(activeServer, 'Default');

        // FIX: when `autoConnectFastest` is enabled, forward the other
        // known servers as `alternativeServers` so the Rust backend can
        // build a `urltest` group and auto-select the fastest outbound.
        const altServers: ReturnType<typeof toRustServerProfile>[] =
          settings.autoConnectFastest && activeServer
            ? servers
                .filter((s) => s.id !== activeServer.id)
                .map((s) => toRustServerProfile(s, 'Default'))
            : [];

        // Save the session servers: primary first, then alternatives.
        // Used by startHealthCheck to map proxy-0, proxy-1 tags back to
        // real server objects after Sing-Box seamlessly failsover.
        const sessionServers = activeServer
          ? [activeServer, ...servers.filter((s) => s.id !== activeServer.id).slice(0, 15)]
          : [];
        set({ _sessionServers: sessionServers });
        logToggle(`_sessionServers set to ${String(sessionServers.length)} server(s)`);

        logToggle(`calling invoke('start_proxy') with port=${String(settings.proxyPort)} mode=${settings.routingMode}`);
        const actualPort = await invoke<number>('start_proxy', {
          profile,
          alternativeServers: altServers,
          routingMode: settings.routingMode,
          proxyPort: settings.proxyPort,
        });
        logToggle(`invoke('start_proxy') returned port=${String(actualPort)}`);

        // Connection established — allow user to disconnect immediately
        // even during the outlet IP verification that follows.
        set({ _isConnecting: false });

        // Save the actual port (may differ if port fallback was triggered)
        if (actualPort !== settings.proxyPort) {
          get().addLog(
            `Proxy port updated to ${actualPort} (fallback from ${settings.proxyPort})`,
            'info',
            'system',
          );
          set((state) => ({
            settings: { ...state.settings, proxyPort: actualPort },
          }));
          logToggle(`port persisted: ${String(actualPort)}`);
          // Persist the new port
          void get().saveSettings();
        }

        // With urltest/fallback groups, Sing-Box handles failover internally.
        // No need to force-select proxy-0 — urltest picks the fastest automatically.
        logToggle('urltest/fallback active — no manual selector switch needed');

        // Outlet IP verification is handled by Rust backend during start_proxy.
        // It checks CDN status from profile manager, skips verification for CDN
        // servers, logs results to JSONL, and emits exit-ip-changed event.
        logToggle('outlet IP verification handled by Rust backend — listening for exit-ip-changed event');

      } else {
        logToggle('DISCONNECT path');
        addLog('Disconnecting...', 'info', 'system');
        set({ status: 'disconnecting', _isConnecting: false });
        logToggle('status set to disconnecting, calling stop_proxy');
        await invoke('stop_proxy');
        logToggle('invoke(stop_proxy) returned — status event should follow');
      }
    } catch (e: unknown) {
      logToggle(`EXCEPTION: ${e instanceof Error ? e.message : String(e)}`, 'error');
      set({ status: 'error', _isConnecting: false });
      const msg = e instanceof Error ? e.message : String(e);
      addLog(`Connection failed: ${msg}`, 'error', 'system');
      throw e;
    } finally {
      logToggle(`finally — resetting _isConnecting (was=${String(get()._isConnecting)})`);
      set({ _isConnecting: false });
    }
  },

  initListener: async () => {
    // ── UI Sync Check — monitors Sing-Box failover and process health ──────
    // Now that Sing-Box handles failover internally (urltest/fallback),
    // this check no longer calls stop_proxy + start_proxy to switch servers.
    // Instead it:
    //   1. Checks if the sing-box process is still alive
    //   2. Queries outlet_ip for the current proxy_tag to detect seamless
    //      failover (proxy-1, proxy-2) and updates the UI accordingly
    const startHealthCheck = () => {
      if (get()._healthCheckTimer) return; // already running

      const timer = setInterval(() => {
        void (async () => {
          const state = useAppStore.getState();
          if (state.status !== 'connected' || state._isReconnecting) {
            return;
          }

          try {
            // 1. Check if the process is still alive.
            //    Rust logs the failure reason via check_proxy_health.
            //    That message propagates to UI via the log-entry event.
            const alive = await invoke<boolean>('check_proxy_health');
            if (!alive) {
              set({ status: 'disconnected', connectedAt: null });
              await invoke('stop_proxy').catch(() => {});
              return;
            }

            // 2. Query current exit IP + proxy tag from Clash API.
            //    Sing-Box's urltest/fallback group reports the active
            //    outbound tag (proxy-0, proxy-1, etc.) in its response.
            const exitInfo = await invoke<ExitIpInfo>('outlet_ip').catch(() => null);

            if (exitInfo && exitInfo.proxy_tag) {
              const tagMatch = exitInfo.proxy_tag.match(/proxy-(\d+)/);
              if (tagMatch && tagMatch[1]) {
                const tagIndex = parseInt(tagMatch[1], 10);

                // If tagIndex > 0, Sing-Box autonomously failed over.
                if (tagIndex > 0) {
                  const sessionServers = get()._sessionServers;
                  const actualActiveServer = sessionServers[tagIndex];

                  if (actualActiveServer && state.activeServer?.id !== actualActiveServer.id) {
                    state.addLog(
                      `Seamless failover: Switched to ${actualActiveServer.name} (proxy-${String(tagIndex)})`,
                      'success',
                      'system',
                    );
                    set({ activeServer: actualActiveServer, exitIp: exitInfo });
                  }
                }
              }
            }
          } catch (e) {
            // Log at info level — transient errors (e.g. Clash API flip)
            // are expected and shouldn't trigger reconnection logic.
            console.warn('UI Sync check failed', e);
          }
        })();
      }, 15000); // Every 15 seconds

      set({ _healthCheckTimer: timer });
    };

    const stopHealthCheck = () => {
      const timer = get()._healthCheckTimer;
      if (timer) {
        clearInterval(timer);
        set({ _healthCheckTimer: null });
      }
    };

    // ── Initial load ─────────────────────────────────────────────────────
    try {
      const rawStatus = await invoke('get_status');
      if (typeof rawStatus === 'string') {
        set({ status: rawStatus as ConnectionStatus });
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      get().addLog(`Failed to get initial proxy status: ${msg}`, 'warning', 'system');
      set({ status: 'disconnected' });
    }

    // Load persistent log history
    try {
      const history = await invoke<LogEntry[]>('get_logs');
      if (history.length > 0) {
        set({ logs: history });
      }
    } catch (e) {
      get().addLog('get_logs failed: ' + String(e), 'warning', 'system');
    }

    const unlisten = await listen<StatusPayload>('connection-status-changed', (event) => {
      const payload = event.payload;
      const errorMsg = extractError(payload);
      const logToggle = (msg: string, level: 'info' | 'warning' | 'error' | 'success' = 'info') => {
        useAppStore.getState().addLog(`[evt:connection-status] ${msg}`, level, 'system');
      };
      logToggle(`received — payload=${JSON.stringify(payload)} currentStatus=${get().status}`);
      if (errorMsg) {
        logToggle(`error: ${errorMsg}`, 'error');
        set({ status: 'error', connectedAt: null });
        stopHealthCheck();
      } else {
        const newStatus = mapConnectionStatus(payload);
        logToggle(`mapped to "${newStatus}" — was "${get().status}"`);
        set({
          status: newStatus,
          connectedAt: newStatus === 'connected' ? Date.now() : null,
        });
        if (newStatus === 'connected') {
          const srv = get().activeServer;
          logToggle(`Connected — activeServer: ${srv?.name ?? 'null'} (${srv?.id.slice(0, 8) ?? 'null'})`);
          startHealthCheck();
        } else {
          stopHealthCheck();
        }
      }
    });

    // Listen for real-time log entries from the Rust backend
    const unlistenLogs = await listen<LogEntry>('log-entry', (event) => {
      const entry = event.payload;
      get().addLog(entry.message, entry.level, entry.source);
    });
    // Listen for real-time traffic stats from sing-box
    const unlistenTraffic = await listen<TrafficPayload>('traffic-update', (event) => {
      set({ speeds: { up: event.payload.up, down: event.payload.down } });
    });

    // Listen for exit IP changes emitted by Rust after proxy verification
    const unlistenExitIp = await listen<ExitIpInfo>('exit-ip-changed', (event) => {
      set({ exitIp: event.payload });
    });

    // Listen for batch refresh progress from the BatchingEngine
    const unlistenRefresh = await listen<RefreshProgress>('refresh-progress', (event) => {
      const progress = event.payload;
      set((state) => ({
        refreshProgress: {
          ...state.refreshProgress,
          [progress.profile_id]: progress.phase === 'done' ? null : progress,
        },
      }));
    });

    return () => {
      stopHealthCheck();
      unlisten();
      unlistenLogs();
      unlistenTraffic();
      unlistenExitIp();
      unlistenRefresh();
    };
  },
}));
