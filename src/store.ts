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
import {getErrorMessage, localizeError} from './lib/errors';
import type {
  ConnectionInfo,
  ConnectionStatus,
  ExitIpInfo,
  LogEntry,
  LogSource,
  Profile,
  RealIpInfo,
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
  appendLog: (entry: LogEntry) => void;
  clearLogs: () => void;
  completeOnboarding: () => void;
  acceptTerms: () => void;
  setSpeeds: (speeds: { up: number; down: number }) => void;
  toggleConnection: () => Promise<void>;
  initListener: () => Promise<() => void>;
  exitIp: ExitIpInfo | null;
  fetchExitIp: () => Promise<void>;
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
    return servers.find((s) => s.id === currentActiveId) ?? servers[0] ?? null;
  }
  const best = await invoke<RustServerProfile | null>('pick_replacement_server', {
    current_server_id: '',
  }).catch(() => null);
  if (best) {
    return servers.find((s) => s.id === best.id) ?? null;
  }
  return servers[0] ?? null;
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
  _isReconnecting: false,
  _isConnecting: false,
  _refreshingIds: [],
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
    const result = await invoke<RustSubscriptionProfile>('add_subscription', {
      name: data.name,
      url: data.url,
    });
    const profile = mapRustProfile(result);

    // Compute best server from current state snapshot, then use functional
    // set() for profiles/servers to avoid losing concurrent updates.
    const newActive = await getBestServer(
      [...get().servers, ...profile.servers],
      get().settings.autoConnectFastest,
      get().activeServer?.id,
    );

    set((state) => ({
      profiles: [...state.profiles, profile],
      servers: [...state.servers, ...profile.servers],
      activeServer: newActive,
    }));
  },

  updateProfile: (id, data) => {
    set((state) => ({
      profiles: state.profiles.map((p) => (p.id === id ? { ...p, ...data } : p)),
    }));
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

    // Functional set: profiles/servers always based on latest state
    set((state) => ({
      profiles: state.profiles.filter((p) => p.id !== id),
      servers: state.servers.filter((s) => !removedIds.has(s.id)),
      activeServer: newActive,
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
      // Backend already logged the error via tracing
      // Reset lastUpdated so timer doesn't retry every ~55s.
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
      const { settings } = get();
      // Показываем toast ДО loadProfiles, чтобы пользователь сразу видел
      // обратную связь. loadProfiles может быть медленным (много профилей,
      // фильтрация по настройкам), и без этого шага toast о загрузке БД
      // появляется с заметной задержкой после клика Save.
      let downloadToastId: string | number | undefined;
      if (settings.ipinfoToken) {
        downloadToastId = toast.loading(i18n.t('ipinfoDownloading'));
      }

      await invoke('update_settings', { newSettings: settings });
      await get().loadProfiles();

      if (settings.ipinfoToken && downloadToastId) {
        try {
          const result = await invoke<string>('download_ipinfo_db');
          if (result === 'downloaded') {
            toast.success(i18n.t('ipinfoDownloaded'), { id: downloadToastId });
          } else {
            toast.info(i18n.t('ipinfoCached'), { id: downloadToastId });
          }
        } catch (e) {
          toast.error(`${i18n.t('ipinfoDownloadFailed')}: ${getErrorMessage(e)}`, { id: downloadToastId });
        }
      }
    } catch (e: unknown) {
      // Backend already logged the error via tracing::error!
      toast.error(localizeError(e, 'settingsSaveFailed'));
    }
  },

  loadSettings: async () => {
    try {
      const result = await invoke<Settings>('get_settings');
      set({ settings: result });
    } catch (e) {
      get().addLog(
        `Failed to load settings: ${getErrorMessage(e)}`,
        'error',
        'settings',
      );
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
    set((state) => {
      // Dedup: entry may already exist from a prior log-entry event
      if (state.logs.some((l) => l.id === entry.id)) return state;
      return {
        logs: [entry, ...state.logs].slice(0, 1000),
      };
    });
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
    } catch {
      // Backend already logged the error via tracing
      set({ exitIp: null });
    }
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

    if (_isReconnecting || _isConnecting) {
      return;
    }

    try {
      if (status === 'disconnected' || status === 'error') {
        set({ _isConnecting: true });
        set({ status: 'connecting' });

        // Persist settings before connecting (routingMode, proxyPort from Dashboard)
        await get().saveSettings();

        const profile = toRustServerProfile(activeServer, 'Default');
        const altServers: ReturnType<typeof toRustServerProfile>[] =
          settings.autoConnectFastest && activeServer
            ? servers
                .filter((s) => s.id !== activeServer.id)
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
      } else {
        set({ status: 'disconnecting', _isConnecting: false });
        await invoke('stop_proxy');
      }
    } catch (e: unknown) {
      set({ status: 'error', _isConnecting: false });
      // Лог: dev-сообщение (англ., без [object Object])
      get().addLog(`Connection error: ${getErrorMessage(e)}`, 'error', 'system');
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

    // Listen for seamless failover — Rust watchdog emits this when sing-box
    // switches to an alternative outbound. Passes exact server ID (no index guessing).
    const unlistenFailover = await listen<string>('failover-occurred', (event) => {
      const newServerId = event.payload;
      const { servers, activeServer, addLog, fetchExitIp } = get();
      if (activeServer?.id !== newServerId) {
        const newServer = servers.find((s) => s.id === newServerId);
        if (newServer) {
          addLog(`Seamless failover: Switched to ${newServer.name}`, 'success', 'system');
          set({ activeServer: newServer });
          void fetchExitIp();
        }
      }
    });

    return () => {
      unlisten();
      unlistenLogs();
      unlistenTraffic();
      unlistenExitIp();
      unlistenRefresh();
      unlistenFailover();
    };
  },
}));
