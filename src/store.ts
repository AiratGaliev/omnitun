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

import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { i18n } from './i18n';
import { getErrorMessage, localizeError } from './lib/errors';
import type {
  ConnectionInfo,
  ConnectionStatus,
  LogEntry,
  LogSource,
  Profile,
  RealIpInfo,
  RefreshProgress,
  Server,
  Settings,
  SpeedTestResult,
} from './types';
import { getCountryName } from './lib/countryNames';

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
  last_tested?: string;
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
  /** Import a subscription from raw content (file or pasted data).
   * Uses the same pending-profile pattern as addProfile: the profile appears
   * in the list immediately, and gets servers after process_servers completes. */
  importProfileContent: (data: { name: string; content: string }) => Promise<void>;
  updateProfile: (id: string, data: Partial<Profile>) => void;
  removeProfile: (id: string) => Promise<void>;
  refreshProfile: (id: string) => Promise<void>;
  /** Test existing servers without re-fetching the subscription URL. */
  testServers: (id: string) => Promise<void>;
  loadProfiles: () => Promise<void>;
  updateSettings: (settings: Partial<Settings>) => void;
  saveSettings: () => Promise<void>;
  /**
   * Persist settings ONLY — no loadProfiles(), no ipinfo download.
   * Use on the hot path of toggleConnection so that a concurrent
   * refresh_subscription or ipinfo download cannot block
   * invoke('start_proxy').
   */
  saveSettingsPersistOnly: () => Promise<void>;
  loadSettings: () => Promise<void>;
  addLog: (message: string, level?: LogEntry['level'], source?: LogSource) => void;
  appendLog: (entry: LogEntry) => void;
  clearLogs: () => void;
  completeOnboarding: () => void;
  acceptTerms: () => void;
  setSpeeds: (speeds: { up: number; down: number }) => void;
  toggleConnection: () => Promise<void>;
  initListener: () => Promise<() => void>;
  /** Current proxy connection phase (null when idle). */
  proxyProgress: { phase: string; message: string } | null;
  realIp: RealIpInfo | null;
  fetchRealIp: () => Promise<void>;
  speedTestResult: SpeedTestResult | null;
  runSpeedTest: () => Promise<SpeedTestResult | null>;
  connections: ConnectionInfo[];
  refreshConnections: () => Promise<void>;
  _isReconnecting: boolean;
  /** Toggle guard — prevents concurrent connect/disconnect. */
  _isConnecting: boolean;
  /** Set of profile IDs currently being refreshed (dedup guard). */
  _refreshingIds: string[];
  /** Set of profile IDs currently being TESTED (no URL fetch).
   * Prevents overlap with `_refreshingIds`. */
  _testingIds: string[];
  /** Last ipinfoToken that was saved to disk. Guards against re-downloading the
   *  GeoIP DB on every settings save — only downloads when the token changes. */
  _lastIpinfoToken: string;
}

const defaultServers: Server[] = [];

// ── Log entry throttle buffer (UI freeze fix for huge subscriptions) ───────
// See appendLog doc-comment for full rationale. Module-scoped buffer shared
// across store re-creations.
const logEntryBuffer: LogEntry[] = [];
const logEntrySeenIds = new Set<string>();
let logFlushScheduled = false;
const LOG_FLUSH_MS = 200;
const MAX_LOGS = 1000;
// Reference to Zustand's set function, initialized when store is created.
let storeSet: ((fn: (state: ProxyStore) => Partial<ProxyStore>) => void) | null = null;

function scheduleLogFlush() {
  if (logFlushScheduled) return;
  logFlushScheduled = true;
  setTimeout(() => {
    logFlushScheduled = false;
    if (logEntryBuffer.length === 0) return;
    const buffered = logEntryBuffer.splice(0, logEntryBuffer.length);
    if (buffered.length === 0) return;
    const set = storeSet;
    if (!set) return;
    set((state) => {
      const reversed = buffered.reverse();
      const newLogs = [...reversed, ...state.logs].slice(0, MAX_LOGS);
      if (logEntrySeenIds.size > MAX_LOGS * 2) {
        logEntrySeenIds.clear();
        for (const l of newLogs) {
          logEntrySeenIds.add(l.id);
        }
      }
      return { logs: newLogs };
    });
  }, LOG_FLUSH_MS);
}

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

/**
 * Convert a single Rust ServerProfile (snake_case JSON from backend)
 * into the frontend Server shape (camelCase).
 * Extracted from mapRustProfile so it can be reused by the
 * servers-discovered event listener.
 */
export function mapRustServer(s: RustServerProfile): Server {
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
    wgLocalAddress: s.wg_local_address ?? undefined,
    wgAllowedIps: s.wg_allowed_ips ?? undefined,
    wgDns: s.wg_dns ?? undefined,
    wgMtu: s.wg_mtu ?? undefined,
    wgKeepalive: s.wg_keepalive ?? undefined,
    username: s.username ?? undefined,
    behindCdn: s.behind_cdn ?? undefined,
  };
}

export function mapRustProfile(rust: RustSubscriptionProfile): Profile {
  return {
    id: rust.id,
    name: rust.name,
    url: rust.url,
    lastUpdated: Date.parse(rust.updated_at) || Date.now(),
    lastTested: rust.last_tested ? Date.parse(rust.last_tested) || undefined : undefined,
    trafficUsed: rust.traffic_used,
    trafficTotal: rust.traffic_total,
    trafficExpiry: rust.traffic_expiry,
    servers: rust.servers.map(mapRustServer),
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

/**
 * Асинхронно запрашивает у бэкенда лучший сервер с учётом всех фильтров
 * (страны, протоколы, CDN). Устраняет дублирование бизнес-логики на фронтенде.
 * При пустом currentServerId бэкенд выбирает из всего пула (без исключения).
 */
async function getBestServer(
  servers: Server[],
  autoSelect: boolean,
  currentActiveId?: string,
): Promise<Server | null> {
  if (!autoSelect || servers.length === 0) {
    const matched = servers.find((s) => s.id === currentActiveId);
    if (matched) return matched;
    if (servers.length > 0) return servers[0];
    return null;
  }
  const best = await invoke<RustServerProfile | null>('pick_replacement_server', {
    current_server_id: '',
  }).catch(() => null);
  if (best) {
    return servers.find((s) => s.id === best.id) ?? null;
  }
  return servers[0] ?? null;
}

export const useAppStore = create<ProxyStore>((set, get) => {
  storeSet = set;
  return {
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
    proxyProgress: null,
    realIp: null,
    speedTestResult: null,
    connections: [],
    _isReconnecting: false,
    _isConnecting: false,
    _refreshingIds: [],
    _testingIds: [],
    _lastIpinfoToken: '',
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
      autoTestInterval: 3,
      antiDpi: true,
      enableMux: false,
      minimizeToTray: false,
      blockCdnNodes: false,
      ipinfoToken: '',
      tlsInsecure: true,
      testUrl: 'cp.cloudflare.com/generate_204',
      testUseTls: true,
    },

    setStatus: (status) => {
      set({ status });
    },
    setActiveServer: (server) => {
      set({ activeServer: server });
    },
    setServers: (servers) => {
      set((state) => {
        const currentActive = state.activeServer;
        const updatedActive = currentActive
          ? (servers.find((s) => s.id === currentActive.id) ?? currentActive)
          : null;
        return { servers, activeServer: updatedActive };
      });
    },
    setConnectedAt: (timestamp) => {
      set({ connectedAt: timestamp });
    },

    addProfile: async (data) => {
      // Pre-generate the profile ID so the backend can emit refresh-progress
      // and servers-discovered events with the REAL profile_id during
      // add_subscription. The pending profile is added to state.profiles
      // immediately (with empty servers) so the user sees it in the list
      // right away — same UX as clicking "Refresh servers" on an existing
      // profile.
      const profileId = crypto.randomUUID();
      const pendingProfile: Profile = {
        id: profileId,
        name: data.name,
        url: data.url,
        lastUpdated: Date.now(),
        servers: [],
      };

      // Add pending profile + mark as refreshing BEFORE invoking the backend.
      // This way:
      //   - The profile appears in the list immediately
      //   - refresh-progress events (with profile_id === profileId) attach to it
      //   - servers-discovered events (with profile_id === profileId) populate
      //     both state.servers and the profile's servers array after full testing
      //   - The Dashboard can detect "a profile is being refreshed" and block
      //     the server picker / connect button until servers arrive
      set((state) => ({
        profiles: [...state.profiles, pendingProfile],
        _refreshingIds: [...state._refreshingIds, profileId],
      }));

      try {
        const result = await invoke<RustSubscriptionProfile>('add_subscription', {
          name: data.name,
          url: data.url,
          profileId,
        });
        const updated = mapRustProfile(result);

        // Replace the pending profile with the real one (same ID — no
        // replacement needed, just update fields). Rebuild servers from
        // flatMap to deduplicate any servers that arrived via servers-discovered
        // events during the add.
        const stateSnap = get();
        const newProfiles = stateSnap.profiles.map((p) =>
          p.id === profileId ? { ...p, ...updated } : p,
        );
        const allServers = newProfiles.flatMap((p) => p.servers);

        const newActive = await getBestServer(
          allServers,
          stateSnap.settings.autoConnectFastest,
          stateSnap.activeServer?.id,
        );

        set({
          profiles: newProfiles,
          servers: allServers,
          activeServer: newActive,
        });
      } catch (e) {
        // Atomic cleanup: remove pending profile, rebuild servers, AND clear
        // _refreshingIds in a single set() call. This avoids a window where
        // a late servers-discovered flush could add orphaned servers between
        // the catch's set() and the finally's set().
        // Also clear refreshProgress — backend doesn't emit phase:"done" for add.
        set((state) => {
          const remainingProfiles = state.profiles.filter((p) => p.id !== profileId);
          const { [profileId]: _rp, ...remainingProgress } = state.refreshProgress;
          return {
            profiles: remainingProfiles,
            servers: remainingProfiles.flatMap((p) => p.servers),
            _refreshingIds: state._refreshingIds.filter((rid) => rid !== profileId),
            refreshProgress: remainingProgress,
          };
        });
        throw e;
      } finally {
        // Success path: clear _refreshingIds + refreshProgress (on error, catch already did this — no-op)
        set((state) => {
          const { [profileId]: _rp, ...remainingProgress } = state.refreshProgress;
          return {
            _refreshingIds: state._refreshingIds.filter((rid) => rid !== profileId),
            refreshProgress: remainingProgress,
          };
        });
      }
    },

    updateProfile: (id, data) => {
      set((state) => ({
        profiles: state.profiles.map((p) => (p.id === id ? { ...p, ...data } : p)),
      }));
    },

    importProfileContent: async (data) => {
      // Same pending-profile pattern as addProfile: pre-generate profile_id,
      // add the pending profile to the store immediately, then invoke the
      // backend. This way refresh-progress events (which carry the real profile_id)
      // update the pending profile's badge.
      const profileId = crypto.randomUUID();
      const pendingProfile: Profile = {
        id: profileId,
        name: data.name,
        url: `content://${profileId}`,
        lastUpdated: Date.now(),
        servers: [],
      };

      set((state) => ({
        profiles: [...state.profiles, pendingProfile],
        _refreshingIds: [...state._refreshingIds, profileId],
      }));

      try {
        const result = await invoke<RustSubscriptionProfile>('import_subscription_content', {
          name: data.name,
          content: data.content,
          profileId,
        });
        const updated = mapRustProfile(result);

        const stateSnap = get();
        const newProfiles = stateSnap.profiles.map((p) =>
          p.id === profileId ? { ...p, ...updated } : p,
        );
        const allServers = newProfiles.flatMap((p) => p.servers);

        const newActive = await getBestServer(
          allServers,
          stateSnap.settings.autoConnectFastest,
          stateSnap.activeServer?.id,
        );

        set({
          profiles: newProfiles,
          servers: allServers,
          activeServer: newActive,
        });
      } catch (e) {
        // Atomic cleanup: remove pending profile, rebuild servers, AND clear
        // _refreshingIds in a single set() call to avoid leaving orphaned
        // servers from late servers-discovered flushes.
        // Also clear refreshProgress — backend doesn't emit phase:"done" here.
        set((state) => {
          const remainingProfiles = state.profiles.filter((p) => p.id !== profileId);
          const { [profileId]: _rp, ...remainingProgress } = state.refreshProgress;
          return {
            profiles: remainingProfiles,
            servers: remainingProfiles.flatMap((p) => p.servers),
            _refreshingIds: state._refreshingIds.filter((rid) => rid !== profileId),
            refreshProgress: remainingProgress,
          };
        });
        throw e;
      } finally {
        // Success path: clear _refreshingIds + refreshProgress (on error, catch already did this — no-op)
        set((state) => {
          const { [profileId]: _rp, ...remainingProgress } = state.refreshProgress;
          return {
            _refreshingIds: state._refreshingIds.filter((rid) => rid !== profileId),
            refreshProgress: remainingProgress,
          };
        });
      }
    },

    removeProfile: async (id) => {
      await invoke('remove_subscription', { id });

      // removedIds derived from the removed profile — immutable, safe from snapshot
      const removed = get().profiles.find((p) => p.id === id);
      const removedIds = new Set(removed?.servers.map((s) => s.id) ?? []);
      const wasActiveRemoved = removedIds.has(get().activeServer?.id ?? '');

      let newActive = get().activeServer;
      if (wasActiveRemoved) {
        const remaining = get().servers.filter((s) => !removedIds.has(s.id));
        newActive = await getBestServer(remaining, get().settings.autoConnectFastest);
      }

      // Clean up tracking state for the removed profile.
      // Без этого _refreshingIds / _testingIds могут навсегда остаться
      // с мёртвым profile_id, а refreshProgress — с устаревшей записью.
      const { [id]: _removed, ...remainingProgress } = get().refreshProgress;
      set((state) => ({
        profiles: state.profiles.filter((p) => p.id !== id),
        servers: state.servers.filter((s) => !removedIds.has(s.id)),
        activeServer: newActive,
        _refreshingIds: state._refreshingIds.filter((rid) => rid !== id),
        _testingIds: state._testingIds.filter((tid) => tid !== id),
        refreshProgress: remainingProgress,
      }));
    },

    refreshProfile: async (id) => {
      // Guard: prevent duplicate concurrent refreshes for the same profile
      {
        const state = get();
        if (state._refreshingIds.includes(id)) {
          return;
        }
      }
      set((state) => ({ _refreshingIds: [...state._refreshingIds, id] }));
      try {
        const result = await invoke<RustSubscriptionProfile>('refresh_subscription', { id });
        const updated = mapRustProfile(result);

        // Build the new server list before getBestServer — so it picks from post-refresh state
        const stateSnap = get();
        const newProfilesForSelection = stateSnap.profiles.map((p) =>
          p.id === id ? { ...p, ...updated } : p,
        );
        const allServersForSelection = newProfilesForSelection.flatMap((p) => p.servers);
        const newActiveServer = await getBestServer(
          allServersForSelection,
          stateSnap.settings.autoConnectFastest,
          stateSnap.activeServer?.id,
        );

        set((state) => {
          const newProfiles = state.profiles.map((p) => (p.id === id ? { ...p, ...updated } : p));
          const allServers = newProfiles.flatMap((p) => p.servers);
          const activeStillValid = newActiveServer
            ? allServers.some((s) => s.id === newActiveServer.id)
            : false;
          return {
            profiles: newProfiles,
            servers: allServers,
            activeServer: activeStillValid ? newActiveServer : (allServers[0] ?? null),
          };
        });
      } catch (e) {
        // Ставим lastUpdated на NOW (не в будущее), чтобы auto-update таймер
        // не ретраил немедленно. Ручное обновление доступно через кнопку Refresh.
        // В будущее ставить нельзя — это даёт отрицательное «Обновлено -1 мин.».
        set((state) => {
          const { [id]: _rp, ...remainingProgress } = state.refreshProgress;
          return {
            profiles: state.profiles.map((p) =>
              p.id === id ? { ...p, lastUpdated: Date.now() } : p,
            ),
            refreshProgress: remainingProgress,
          };
        });
        throw e;
      } finally {
        set((state) => {
          const { [id]: _rp, ...remainingProgress } = state.refreshProgress;
          return {
            _refreshingIds: state._refreshingIds.filter((rid) => rid !== id),
            refreshProgress: remainingProgress,
          };
        });
      }
    },

    // testServers — test existing servers without URL fetch ──────
    testServers: async (id) => {
      const state = get();
      if (state._testingIds.includes(id)) return;
      if (state._refreshingIds.includes(id)) {
        get().addLog(`Skipping test for profile ${id} — refresh in progress`, 'info', 'cache');
        return;
      }
      set((state) => ({ _testingIds: [...state._testingIds, id] }));
      try {
        const result = await invoke<RustSubscriptionProfile>('test_servers', { id });
        const updated = mapRustProfile(result);
        set((state) => {
          const { [id]: _rp, ...remainingProgress } = state.refreshProgress;
          const newProfiles = state.profiles.map((p) => (p.id === id ? { ...p, ...updated } : p));
          const allServers = newProfiles.flatMap((p) => p.servers);
          return { profiles: newProfiles, servers: allServers, refreshProgress: remainingProgress };
        });
      } catch (e) {
        set((state) => {
          const { [id]: _rp, ...remainingProgress } = state.refreshProgress;
          return {
            profiles: state.profiles.map((p) =>
              p.id === id ? { ...p, lastTested: Date.now() } : p,
            ),
            refreshProgress: remainingProgress,
          };
        });
        throw e;
      } finally {
        set((state) => ({
          _testingIds: state._testingIds.filter((tid) => tid !== id),
        }));
      }
    },

    loadProfiles: async () => {
      try {
        const results = await invoke<RustSubscriptionProfile[]>('get_subscriptions');
        const profiles = results.map(mapRustProfile);
        const allServers = profiles.flatMap((p) => p.servers);
        const state = get();

        const newActive = await getBestServer(
          allServers,
          state.settings.autoConnectFastest,
          state.activeServer?.id,
        );

        set({
          profiles,
          servers: allServers,
          activeServer: newActive,
          hasOnboarded: profiles.length > 0 || get().hasOnboarded,
        });
      } catch (e) {
        get().addLog(`Failed to load profiles: ${getErrorMessage(e)}`, 'error', 'subscription');
        set({ servers: [], profiles: [] });
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
        const { settings, _lastIpinfoToken } = get();
        const tokenChanged = settings.ipinfoToken !== _lastIpinfoToken;

        // Показываем toast ДО loadProfiles, чтобы пользователь сразу видел
        // обратную связь. loadProfiles может быть медленным (много профилей,
        // фильтрация по настройкам), и без этого шага toast о загрузке БД
        // появляется с заметной задержкой после клика Save.
        let downloadToastId: string | number | undefined;
        const tokenWasRemoved = _lastIpinfoToken && !settings.ipinfoToken;
        if (tokenChanged && settings.ipinfoToken) {
          downloadToastId = toast.loading(i18n.t('ipinfoDownloading'));
        }

        await invoke('update_settings', { newSettings: settings });
        await get().loadProfiles();

        // Token was removed by the user — notify that GeoIP updates are stopped
        if (tokenWasRemoved) {
          toast.info(i18n.t('ipinfoTokenRemoved'), { duration: 5000 });
        } else if (tokenChanged && settings.ipinfoToken && downloadToastId) {
          try {
            const result = await invoke<string>('download_ipinfo_db');
            if (result === 'downloaded') {
              toast.success(i18n.t('ipinfoDownloaded'), { id: downloadToastId });
            } else {
              toast.info(i18n.t('ipinfoCached'), { id: downloadToastId });
            }
          } catch (e) {
            toast.error(`${i18n.t('ipinfoDownloadFailed')}: ${getErrorMessage(e)}`, {
              id: downloadToastId,
            });
          }
        }

        // Update the guard so subsequent saves don't re-download
        set({ _lastIpinfoToken: settings.ipinfoToken });
      } catch (e: unknown) {
        // Backend already logged the error via tracing::error!
        toast.error(localizeError(e, 'settingsSaveFailed'));
      }
    },

    // ─────────────────────────────────────────────────────────────────────
    // saveSettingsPersistOnly: minimum-overhead settings persistence.
    //
    // Used by toggleConnection() so the connect button is not blocked by
    // a concurrent refresh_subscription (ProfileManager mutex) or ipinfo
    // DB download (10-30 s HTTP).
    //
    // Does NOT call loadProfiles() and does NOT trigger ipinfo DB
    // download. Both are deferred / optional.
    // ─────────────────────────────────────────────────────────────────────
    saveSettingsPersistOnly: async () => {
      try {
        const { settings } = get();
        await invoke('update_settings', { newSettings: settings });
        // Remember the ipinfo token guard so a subsequent full
        // saveSettings() call doesn't re-download.
        set({ _lastIpinfoToken: settings.ipinfoToken });
      } catch (e: unknown) {
        toast.error(localizeError(e, 'settingsSaveFailed'));
        throw e;
      }
    },

    loadSettings: async () => {
      try {
        const result = await invoke<Settings | null>('get_settings');
        if (result) {
          set({ settings: result });
        }
      } catch (e) {
        get().addLog(`Failed to load settings: ${getErrorMessage(e)}`, 'error', 'settings');
        set({ settingsVersion: get().settingsVersion });
      }
    },

    addLog: (message: string, level: LogEntry['level'] = 'info', source: LogSource = 'system') => {
      // Send to Rust backend — it owns id/timestamp and writes to JSONL.
      // The entry comes back via log-entry event → appendLog for state update.
      invoke('log_frontend', { message, level, source }).catch(() => {
        // Dev fallback (browser without Tauri IPC): append locally
        get().appendLog({
          id: crypto.randomUUID(),
          timestamp: Date.now(),
          message,
          level,
          source,
        });
      });
    },

    appendLog: (entry) => {
      // ── THROTTLE (UI freeze fix for huge subscriptions) ────────────────
      // Previously, every log-entry event triggered a synchronous set() with
      // [entry, ...state.logs].slice(0, 1000). For a huge subscription refresh
      // (7000+ servers), the batch-test phase emits hundreds of log entries in
      // seconds. Each set() caused:
      //   (a) O(n) dedup scan via state.logs.some() — up to 1000 comparisons
      //   (b) O(n) array spread — copies up to 1000 elements
      //   (c) Zustand subscriber notification → re-render of every component
      //       subscribed to the store via useAppStore() (Dashboard, Profiles,
      //       Logs, settings screens — all without selectors)
      //
      // Combined cost: 500 entries × 1000-element scan = 500K comparisons,
      // 500 array spreads of 1000 elements, 500 re-render cascades.
      //
      // We now buffer entries in a module-scoped array and flush on a ~200ms
      // cadence. Dedup is done via a Set of IDs (O(1) per check). The flush
      // prepends all buffered entries in a single set(), reducing 500 set()
      // calls to ~1-2 per 200ms window. This keeps the UI responsive even
      // during a 7000-server batch test that logs hundreds of failures.
      logEntryBuffer.push(entry);
      logEntrySeenIds.add(entry.id);
      scheduleLogFlush();
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

    fetchRealIp: async () => {
      try {
        const info = await invoke<RealIpInfo>('get_real_ip');
        set({ realIp: info });
      } catch {
        // Backend already logged the error via tracing
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
        // Backend already logged the error via tracing
        set({ speedTestResult: null });
        return null;
      }
    },

    refreshConnections: async () => {
      try {
        const conns = await invoke<ConnectionInfo[]>('get_connections');
        set({ connections: conns });
      } catch {
        // Clash API unavailable — reset connections list
        set({ connections: [] });
      }
    },

    toggleConnection: async () => {
      const { status, activeServer, servers, settings, _isReconnecting, _isConnecting } = get();

      // ── Cancel an in-progress connection ───────────────────────────
      if (_isConnecting || status === 'connecting') {
        set({ status: 'disconnecting', _isConnecting: false });
        try {
          await invoke('cancel_connect');
        } catch (e: unknown) {
          // Backend may already have transitioned; treat as best-effort.
          get().addLog(`Cancel connect failed: ${getErrorMessage(e)}`, 'warning', 'system');
        }
        set({ proxyProgress: null });
        set({ status: 'disconnected' });
        return;
      }

      // Don't allow toggling during seamless routing-mode reconnect
      // (separate code path in Dashboard.tsx seamlessReconnect).
      if (_isReconnecting) {
        return;
      }

      try {
        if (status === 'disconnected' || status === 'error') {
          set({ _isConnecting: true });
          set({ status: 'connecting' });

          // Persist settings before connecting; use lightweight variant
          // to avoid blocking on a concurrent refresh or ipinfo download.
          await get().saveSettingsPersistOnly();

          const profile = toRustServerProfile(activeServer, 'Default');
          // ── CAP alternativeServers (UI freeze + sing-box startup fix) ─────
          // For a huge subscription (7000+ servers), passing ALL servers as
          // alternatives causes two problems:
          //   (a) The IPC payload from frontend to Rust is a multi-MB JSON
          //       blob of 7000 ServerProfile objects — serializing it blocks
          //       the main thread for hundreds of milliseconds.
          //   (b) Rust builds a sing-box config with 7000+ outbounds in a
          //       urltest group — sing-box either rejects it, takes very long
          //       to start, or consumes excessive memory.
          //
          // We cap to the top 50 by ping (servers are already sorted by ping
          // ascending from process_servers). 50 alternatives is more than
          // enough for failover.
          const MAX_ALT_SERVERS = 50;
          const altServers: ReturnType<typeof toRustServerProfile>[] =
            settings.autoConnectFastest && activeServer
              ? servers
                  .filter((s) => s.id !== activeServer.id)
                  .slice(0, MAX_ALT_SERVERS)
                  .map((s) => toRustServerProfile(s, 'Default'))
              : [];

          const actualPort = await invoke<number>('start_proxy', {
            profile,
            alternativeServers: altServers,
            routingMode: settings.routingMode,
            proxyPort: settings.proxyPort,
          });

          // Save the actual port (may differ if port fallback was triggered)
          if (actualPort !== settings.proxyPort) {
            set((state) => ({
              settings: { ...state.settings, proxyPort: actualPort },
            }));
            void get().saveSettings();
          }

          // Deferred, non-blocking: refresh the server list so any filter
          // changes the user just made eventually propagate. Wrapped so a
          // failure here can never affect the connection result.
          void get()
            .loadProfiles()
            .catch(() => {
              /* swallow — loadProfiles will retry on the next 60 s timer */
            });
        } else {
          set({ status: 'disconnecting', _isConnecting: false });
          await invoke('stop_proxy');
        }
      } catch (e: unknown) {
        // If the user cancelled while start_proxy was in-flight, suppress
        // the error — the cancel_connect already set status to 'disconnected'.
        // Avoid any set() call — status is already correct, a redundant set
        // would trigger a Zustand re-render and cause visual flicker.
        const cancelled =
          get().status === 'disconnected' ||
          get().status === 'disconnecting' ||
          getErrorMessage(e).toLowerCase().includes('cancelled');
        if (cancelled) {
          return;
        }
        set({ status: 'error', _isConnecting: false });
        // Backend already logged the error via tracing::error!
        // Тост: локализованное сообщение
        // Для RustError (processNotReachable и др.) — перевод по коду.
        // Для прочих — connectionFailedDesc.
        toast.error(localizeError(e, 'connectionFailedDesc'));
      } finally {
        set({ _isConnecting: false });
      }
    },

    initListener: async () => {
      // ── Initial load ─────────────────────────────────────────────────────
      try {
        const rawStatus = await invoke('get_status');
        if (typeof rawStatus === 'string') {
          set({ status: rawStatus as ConnectionStatus });
        }
      } catch {
        // Backend already logged the error via tracing
        set({ status: 'disconnected' });
      }

      // Load persistent log history
      try {
        const history = await invoke<LogEntry[]>('get_logs');
        if (history.length > 0) {
          set({ logs: history });
        }
      } catch {
        // first launch — no log file yet, logs stay empty
        set({ logs: [] });
      }

      const unlisten = await listen<StatusPayload>('connection-status-changed', (event) => {
        const payload = event.payload;
        const errorMsg = extractError(payload);
        if (errorMsg) {
          set({ status: 'error', connectedAt: null });
        } else {
          const newStatus = mapConnectionStatus(payload);
          set({
            status: newStatus,
            connectedAt: newStatus === 'connected' ? Date.now() : null,
          });
          // Warn about disabled Kill Switch on first connect
          if (newStatus === 'connected' && !get().settings.killSwitch) {
            toast.warning(i18n.t('killSwitchOffWarning'), {
              description: i18n.t('killSwitchOffWarningDesc'),
              duration: 8000,
              action: {
                label: i18n.t('enable'),
                onClick: () => {
                  get().updateSettings({ killSwitch: true });
                  void get().saveSettings();
                },
              },
            });
          }
        }
      });

      // Listen for real-time log entries from the Rust backend
      const unlistenLogs = await listen<LogEntry>('log-entry', (event) => {
        get().appendLog(event.payload);
      });
      // Listen for real-time traffic stats from sing-box
      const unlistenTraffic = await listen<TrafficPayload>('traffic-update', (event) => {
        set({ speeds: { up: event.payload.up, down: event.payload.down } });
      });

      // Listen for exit IP changes emitted by Rust after proxy verification
      // Listen for batch refresh progress from the BatchingEngine.
      //
      // ── THROTTLE (UI freeze fix) ──────────────────────────────────────────
      // refresh-progress events fire once per server tested. With many profiles
      // × many servers, hundreds of events arrive in seconds. Each event
      // triggers a Zustand set() → re-render of every screen subscribed to the
      // store (Profiles, Dashboard, Logs all use useAppStore()). Without
      // throttling, the main thread is saturated with set() + React
      // reconciliation calls and the UI freezes.
      //
      // We buffer the latest progress per profile_id and flush on a ~120ms
      // cadence. This coalesces rapid-fire events into at most ~8 store
      // updates per second, keeping the UI responsive while still showing
      // live progress.
      const refreshProgressBuffer = new Map<string, RefreshProgress | null>();
      let refreshProgressFlushScheduled = false;
      const REFRESH_PROGRESS_FLUSH_MS = 120;

      const flushRefreshProgress = () => {
        refreshProgressFlushScheduled = false;
        if (refreshProgressBuffer.size === 0) return;
        const buffered = new Map(refreshProgressBuffer);
        refreshProgressBuffer.clear();
        set((state) => {
          const next = { ...state.refreshProgress };
          let profilesUpdated = false;
          const updatedProfiles = state.profiles.map((p) => {
            const prog = buffered.get(p.id);
            if (prog?.phase === 'done' && prog.updated_at) {
              profilesUpdated = true;
              return {
                ...p,
                lastUpdated: Date.parse(prog.updated_at),
                lastTested: prog.last_tested
                  ? Date.parse(prog.last_tested) || undefined
                  : undefined,
              };
            }
            return p;
          });
          for (const [id, prog] of buffered) {
            next[id] = prog?.phase === 'done' ? null : prog;
          }
          // eslint-disable-next-line @typescript-eslint/no-unnecessary-condition
          return profilesUpdated
            ? { refreshProgress: next, profiles: updatedProfiles }
            : { refreshProgress: next };
        });
      };

      const scheduleRefreshProgressFlush = () => {
        if (refreshProgressFlushScheduled) return;
        refreshProgressFlushScheduled = true;
        setTimeout(flushRefreshProgress, REFRESH_PROGRESS_FLUSH_MS);
      };

      const unlistenRefresh = await listen<RefreshProgress>('refresh-progress', (event) => {
        const progress = event.payload;
        refreshProgressBuffer.set(progress.profile_id, progress);
        scheduleRefreshProgressFlush();
      });

      // Listen for proxy connection progress
      const unlistenProxyProgress = await listen<{ phase: string; message: string }>(
        'proxy-progress',
        (event) => {
          set({ proxyProgress: event.payload });
        },
      );

      // Listen for seamless failover — Rust watchdog emits this when sing-box
      // switches to an alternative outbound. Passes exact server ID (no index guessing).
      const unlistenFailover = await listen<string>('failover-occurred', (event) => {
        const newServerId = event.payload;
        const { servers, activeServer } = get();
        if (activeServer?.id !== newServerId) {
          const newServer = servers.find((s) => s.id === newServerId);
          if (newServer) {
            // Backend already logged the failover via tracing::info!
            // ("Seamless failover detected: {current_tag} -> {now_tag}")
            set({ activeServer: newServer });
          }
        }
      });

      // Listen for servers-discovered ────────────────────────────
      // Backend emits fully-tested servers in BATCH_SIZE groups after
      // TCP ping + batch test complete. Frontend merges by server ID.
      const serversDiscoveredBuffer: { profileId: string; servers: RustServerProfile[] }[] = [];
      let serversDiscoveredFlushScheduled = false;
      const SERVERS_DISCOVERED_FLUSH_MS = 150;

      const flushServersDiscovered = () => {
        serversDiscoveredFlushScheduled = false;
        if (serversDiscoveredBuffer.length === 0) return;
        const chunks = serversDiscoveredBuffer.splice(0, serversDiscoveredBuffer.length);
        set((state) => {
          const knownProfileIds = new Set(state.profiles.map((p) => p.id));
          const validChunks = chunks.filter(({ profileId }) => knownProfileIds.has(profileId));
          if (validChunks.length === 0) return {};

          const existing = new Map(state.servers.map((s) => [s.id, s]));
          for (const { servers } of validChunks) {
            for (const rs of servers) {
              const mapped = mapRustServer(rs);
              existing.set(mapped.id, mapped);
            }
          }
          const newServers = Array.from(existing.values());
          newServers.sort((a, b) => (a.ping ?? Infinity) - (b.ping ?? Infinity));
          const currentActive = state.activeServer;
          const updatedActive = currentActive
            ? (newServers.find((s) => s.id === currentActive.id) ?? currentActive)
            : null;

          const byProfile = new Map<string, Map<string, Server>>();
          for (const { profileId, servers } of validChunks) {
            let bucket = byProfile.get(profileId);
            if (!bucket) {
              bucket = new Map();
              byProfile.set(profileId, bucket);
            }
            for (const rs of servers) {
              const mapped = mapRustServer(rs);
              bucket.set(mapped.id, mapped);
            }
          }
          const newProfiles = state.profiles.map((p) => {
            const bucket = byProfile.get(p.id);
            if (!bucket) return p;
            const profileExisting = new Map(p.servers.map((s) => [s.id, s]));
            for (const [id, s] of bucket) {
              profileExisting.set(id, s);
            }
            const mergedServers = Array.from(profileExisting.values());
            mergedServers.sort((a, b) => (a.ping ?? Infinity) - (b.ping ?? Infinity));
            return { ...p, servers: mergedServers };
          });

          return { servers: newServers, activeServer: updatedActive, profiles: newProfiles };
        });
      };

      const scheduleServersDiscoveredFlush = () => {
        if (serversDiscoveredFlushScheduled) return;
        serversDiscoveredFlushScheduled = true;
        setTimeout(flushServersDiscovered, SERVERS_DISCOVERED_FLUSH_MS);
      };

      const unlistenServersDiscovered = await listen<{
        profile_id: string;
        servers: RustServerProfile[];
      }>('servers-discovered', (event) => {
        const chunk = event.payload.servers;
        if (chunk.length === 0) return;
        serversDiscoveredBuffer.push({ profileId: event.payload.profile_id, servers: chunk });
        scheduleServersDiscoveredFlush();
      });

      return () => {
        unlisten();
        unlistenLogs();
        unlistenTraffic();
        unlistenRefresh();
        unlistenProxyProgress();
        unlistenFailover();
        unlistenServersDiscovered();
      };
    },
  };
});

/**
 * Get display name for a server, with localized country name.
 *
 * Всегда использует getCountryName(flag, locale) для локализации,
 * даже когда countryName пришёл с бэкенда (ipinfo Lite / MaxMind City).
 * countryName используется как fallback, если flag отсутствует.
 * Города не локализуются — MaxMind отдаёт их по-английски.
 *
 * @param server — server profile (любой объект с countryName/flag/etc.)
 * @param locale — current i18next language ('en' | 'ru')
 * @returns display name: 'Country, City' | 'Country' | original_name | 'host:port'
 */
export function getServerDisplayName(
  server:
    | {
        countryName?: string | null;
        flag?: string | null;
        name?: string;
        server?: string;
        port?: number;
      }
    | null
    | undefined,
  locale = 'en',
): string {
  if (!server) return '';
  // 1. Если есть flag (country_code) — локализуем через i18n-iso-countries
  //    Всегда, даже если countryName пришёл с бэкенда.
  if (server.flag && server.flag.length > 0) {
    return getCountryName(server.flag, locale);
  }
  // 2. Fallback на countryName с бэкенда (без локализации, только English)
  if (server.countryName && server.countryName.length > 0) {
    return server.countryName;
  }
  // 3. Fallback на name (display_name из бэкенда или original_name)
  if (server.name && server.name.length > 0) {
    return server.name;
  }
  // 4. Последний fallback — host:port
  return `${server.server ?? ''}:${String(server.port ?? '')}`;
}

/**
 * Get display name for exit IP info (when connected through proxy).
 * Аналогично getServerDisplayName — всегда локализует через flag.
 */
export function getExitIpDisplayName(
  exitIp:
    | {
        server?: string | null;
        country_name?: string | null;
        flag?: string | null;
      }
    | null
    | undefined,
  locale = 'en',
): string {
  if (!exitIp) return '';
  // 1. flag доступен — локализуем через i18n-iso-countries
  if (exitIp.flag && exitIp.flag.length > 0) {
    return getCountryName(exitIp.flag, locale);
  }
  // 2. Fallback на country_name с бэкенда (English)
  if (exitIp.country_name && exitIp.country_name.length > 0) {
    return exitIp.country_name;
  }
  // 3. Fallback на server IP
  if (exitIp.server && exitIp.server.length > 0) {
    return exitIp.server;
  }
  return '';
}
