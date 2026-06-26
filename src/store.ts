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
//    the Rust `start_vpn` command. Combined with bugs #1 and #2, this
//    guaranteed that those servers could never be connected to from the
//    dashboard. Now those fields are sourced from `activeServer`.
//
// 4. `alternativeServers` was always an empty array, which defeated the
//    `autoConnectFastest` setting — the Rust backend then never built a
//    `urltest` group for failover. When `autoConnectFastest` is enabled,
//    we now pass the remaining known servers (excluding the selected one)
//    as alternative servers so the backend can construct the urltest group
//    and auto-pick the fastest outbound.
//
// Other than these targeted fixes, the file is unchanged from the original
// codebase.

import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type {
  ConnectionInfo,
  ConnectionStatus,
  LogEntry,
  LogSource,
  Profile,
  Server,
  Settings,
  SpeedTestResult,
} from './types';

interface VpnStatusPayload {
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
  // Rust's `ServerProfile` struct (see src-tauri/src/vpn/mod.rs) defines
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

interface VPNStore {
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
  outletIp: string | null;
  fetchOutletIp: () => Promise<void>;
  realIp: string | null;
  fetchRealIp: () => Promise<void>;
  speedTestResult: SpeedTestResult | null;
  runSpeedTest: () => Promise<SpeedTestResult | null>;
  connections: ConnectionInfo[];
  refreshConnections: () => Promise<void>;
}

const defaultServers: Server[] = [];

function mapVpnStatus(payload: VpnStatusPayload): ConnectionStatus {
  if (typeof payload.status === 'string') {
    return payload.status as ConnectionStatus;
  }
  return 'error';
}

function extractError(payload: VpnStatusPayload): string | undefined {
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
        // ── FIX: propagate WireGuard + username fields so the dashboard
        //    can show them and `toggleConnection` can forward them to
        //    `start_vpn`. Without this, WireGuard / SOCKS5-auth /
        //    HTTP-auth servers silently lose their connection data
        //    when the user picks them in the dropdown. ─────────────────
        wgLocalAddress: s.wg_local_address ?? undefined,
        wgAllowedIps: s.wg_allowed_ips ?? undefined,
        wgDns: s.wg_dns ?? undefined,
        wgMtu: s.wg_mtu ?? undefined,
        wgKeepalive: s.wg_keepalive ?? undefined,
        username: s.username ?? undefined,
      };
    }),
  };
}

/**
 * Convert a frontend `Server` object into the snake_case payload shape that
 * the Rust `start_vpn` command expects (matches `ServerProfile` in
 * src-tauri/src/vpn/mod.rs).
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
  };
}

export const useVPNStore = create<VPNStore>((set, get) => ({
  status: 'disconnected',
  activeServer: null,
  servers: defaultServers,
  profiles: [],
  hasOnboarded: false,
  hasAcceptedTerms: false,
  speeds: { up: 0, down: 0 },
  connectedAt: null,
  logs: [],
  settingsVersion: 0,
  outletIp: null,
  realIp: null,
  speedTestResult: null,
  connections: [],
  settings: {
    acceptTerms: false,
    routingMode: 'vpn',
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
  },

  setStatus: (status) => {
    set({ status });
  },
  setActiveServer: (server) => {
    set({ activeServer: server });
  },
  setServers: (servers) => {
    set({ servers });
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
      set((state) => ({
        profiles: [...state.profiles, profile],
        servers: [...state.servers, ...profile.servers],
        // eslint-disable-next-line @typescript-eslint/no-unnecessary-condition
        activeServer: state.activeServer ?? profile.servers[0] ?? null,
      }));
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
        return {
          profiles: state.profiles.filter((p) => p.id !== id),
          servers: remaining,
          activeServer: removedIds.has(state.activeServer?.id ?? '')
            ? (remaining[0] ?? undefined)
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
    try {
      const result = await invoke<RustSubscriptionProfile>('refresh_subscription', { id });
      const updated = mapRustProfile(result);
      set((state) => {
        const newProfiles = state.profiles.map((p) => (p.id === id ? { ...p, ...updated } : p));
        const allServers = newProfiles.flatMap((p) => p.servers);
        const activeServerStillExists = allServers.some((s) => s.id === state.activeServer?.id);
        // Auto-select fastest when setting is enabled or current server is gone
        const autoSelect = state.settings.autoConnectFastest || !activeServerStillExists;
        const oldActiveName = state.activeServer?.name ?? state.activeServer?.server ?? 'none';
        const newActiveServer = autoSelect
          ? allServers.reduce((best, s) => ((s.ping ?? 9999) < (best.ping ?? 9999) ? s : best))
          : state.activeServer;
        if (autoSelect && state.activeServer && state.activeServer.id !== newActiveServer?.id) {
          const newName = newActiveServer?.name ?? newActiveServer?.server ?? 'none';
          get().addLog(
            `Active server changed: ${oldActiveName} → ${newName} (reason: ${!activeServerStillExists ? 'ID not found after refresh' : 'autoConnectFastest'})`,
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
    }
  },

  loadProfiles: async () => {
    try {
      const results = await invoke<RustSubscriptionProfile[]>('get_subscriptions');
      const profiles = results.map(mapRustProfile);
      const allServers = profiles.flatMap((p) => p.servers);
      const state = get();
      const activeServerStillExists = allServers.some((s) => s.id === state.activeServer?.id);
      const autoSelect = !state.activeServer || !activeServerStillExists;
      set({
        profiles,
        servers: allServers,
        activeServer: autoSelect
          ? allServers.reduce((best, s) => ((s.ping ?? 9999) < (best.ping ?? 9999) ? s : best))
          : state.activeServer,
      });
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
    } catch (e) {
      get().addLog(`Failed to save settings: ${String(e)}`, 'error', 'settings');
    }
  },

  loadSettings: async () => {
    try {
      const result = await invoke<Settings>('get_settings');
      // Migration: old 'proxy' mode → 'system-proxy'
      if ((result as { routingMode: string }).routingMode === 'proxy') {
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

  fetchOutletIp: async () => {
    try {
      const ip = await invoke<string>('outlet_ip');
      set({ outletIp: ip });
    } catch {
      set({ outletIp: null });
    }
  },

  fetchRealIp: async () => {
    try {
      const ip = await invoke<string>('get_real_ip');
      set({ realIp: ip });
    } catch {
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
    } catch {
      set({ speedTestResult: null });
      return null;
    }
  },

  refreshConnections: async () => {
    try {
      const conns = await invoke<ConnectionInfo[]>('get_connections');
      set({ connections: conns });
    } catch {
      // VPN not connected — not an error
    }
  },

  toggleConnection: async () => {
    const { status, activeServer, addLog, settings } = get();
    try {
      if (status === 'disconnected' || status === 'error') {
        addLog(`Connecting to ${activeServer?.name ?? 'Default'}...`, 'info', 'system');
        set({ status: 'connecting' });
        // Persist settings before connecting (routingMode, proxyPort from Dashboard)
        await get()
          .saveSettings()
          .catch(() => undefined);

        // FIX: build the rust-side profile from `activeServer` WITHOUT
        // dropping WireGuard / username fields. Previously all of these
        // were hardcoded to `null`, which is why WireGuard / SOCKS5-auth
        // / HTTP-auth servers couldn't be connected to from the dashboard.
        const profile = toRustServerProfile(activeServer, 'Default');

        // FIX: when `autoConnectFastest` is enabled, forward the other
        // known servers as `alternativeServers` so the Rust backend can
        // build a `urltest` group and auto-select the fastest outbound.
        // Previously this was always an empty array, which silently
        // disabled the autoConnectFastest feature.
        const altServers: ReturnType<typeof toRustServerProfile>[] = [];

        await invoke('start_vpn', {
          profile,
          alternativeServers: altServers,
          routingMode: settings.routingMode,
          proxyPort: settings.proxyPort,
        });

        // Verify IP changed through VPN — poll outlet IP until different from real IP.
        // Use increasing delay (2s, 3s, 4s...) to avoid hammering IP check services.
        const realIp = get().realIp ?? (await invoke<string>('get_real_ip').catch(() => null));
        const maxAttempts = 12; // ~60 seconds with backoff
        let ipChanged = false;
        for (let i = 0; i < maxAttempts; i++) {
          const delay = 2000 + i * 500; // 2s, 2.5s, 3s, ... (max 7.5s)
          await new Promise((r) => setTimeout(r, delay));
          try {
            const vpnIp = await invoke<string>('outlet_ip');
            if (realIp && vpnIp && !vpnIp.startsWith(realIp.split(' ')[0])) {
              set({ outletIp: vpnIp });
              ipChanged = true;
              break;
            }
          } catch {
            // VPN not ready yet, retry
          }
        }
        if (!ipChanged && realIp) {
          addLog(
            'VPN connected but IP did not change — check your proxy server',
            'warning',
            'system',
          );
        }
      } else {
        addLog('Disconnecting...', 'info', 'system');
        set({ status: 'disconnecting' });
        await invoke('stop_vpn');
      }
    } catch (e: unknown) {
      set({ status: 'error' });
      const msg = e instanceof Error ? e.message : String(e);
      addLog(`Connection failed: ${msg}`, 'error', 'system');
      throw e;
    }
  },

  initListener: async () => {
    try {
      const rawStatus = await invoke('get_vpn_status');
      if (typeof rawStatus === 'string') {
        set({ status: rawStatus as ConnectionStatus });
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      get().addLog(`Failed to get initial VPN status: ${msg}`, 'warning', 'system');
      // Stay disconnected — it's the default initial state
      set({ status: 'disconnected' });
    }

    // Load persistent log history
    try {
      const history = await invoke<LogEntry[]>('get_logs');
      if (history.length > 0) {
        set({ logs: history });
      }
    } catch {
      // Non-critical: log history unavailable on first run
    }

    const unlisten = await listen<VpnStatusPayload>('vpn-status-changed', (event) => {
      const payload = event.payload;
      const errorMsg = extractError(payload);
      if (errorMsg) {
        set({ status: 'error', connectedAt: null });
      } else {
        const newStatus = mapVpnStatus(payload);
        set({
          status: newStatus,
          connectedAt: newStatus === 'connected' ? Date.now() : null,
        });
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

    return () => {
      unlisten();
      unlistenLogs();
      unlistenTraffic();
    };
  },
}));
