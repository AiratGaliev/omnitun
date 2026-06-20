import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { ConnectionStatus, LogEntry, LogSource, Profile, Server, Settings } from './types';

interface VpnStatusPayload {
  status: string | { error: string };
}

interface RustServerProfile {
  id: string;
  name: string;
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
}

interface RustSubscriptionProfile {
  id: string;
  name: string;
  url: string;
  servers: RustServerProfile[];
  updated_at: string;
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
  logs: LogEntry[];
  settingsVersion: number;

  setStatus: (status: ConnectionStatus) => void;
  setActiveServer: (server: Server) => void;
  setServers: (servers: Server[]) => void;
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

function mapRustProfile(rust: RustSubscriptionProfile): Profile {
  return {
    id: rust.id,
    name: rust.name,
    url: rust.url,
    lastUpdated: Date.parse(rust.updated_at) || Date.now(),
    servers: rust.servers.map(
      (s: RustServerProfile): Server => ({
        id: s.id,
        name: s.name,
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
      }),
    ),
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
  logs: [],
  settingsVersion: 0,
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
      set((state) => ({
        profiles: state.profiles.map((p) => (p.id === id ? { ...p, ...updated } : p)),
      }));
      get().addLog(
        `Profile "${updated.name}" refreshed (${String(updated.servers.length)} server(s))`,
        'success',
        'subscription',
      );
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      get().addLog(`Failed to refresh profile: ${msg}`, 'error', 'subscription');
      throw e;
    }
  },

  loadProfiles: async () => {
    try {
      const results = await invoke<RustSubscriptionProfile[]>('get_subscriptions');
      const profiles = results.map(mapRustProfile);
      const allServers = profiles.flatMap((p) => p.servers);
      const state = get();
      set({
        profiles,
        servers: allServers,
        // eslint-disable-next-line @typescript-eslint/no-unnecessary-condition
        activeServer: state.activeServer ?? allServers[0] ?? null,
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

  toggleConnection: async () => {
    const { status, activeServer, addLog } = get();
    try {
      if (status === 'disconnected' || status === 'error') {
        addLog(`Connecting to ${activeServer?.name ?? 'Default'}...`, 'info', 'system');
        set({ status: 'connecting' });
        const profile = {
          id: activeServer?.id ?? 'default',
          name: activeServer?.name ?? 'Default',
          protocol: activeServer?.protocol ?? 'vless',
          server: activeServer?.server ?? '127.0.0.1',
          port: activeServer?.port ?? 0,
          uuid: activeServer?.uuid ?? null,
          password: activeServer?.password ?? null,
          method: activeServer?.method ?? null,
          sni: activeServer?.sni ?? null,
          flow: activeServer?.flow ?? null,
          public_key: activeServer?.publicKey ?? null,
          private_key: activeServer?.privateKey ?? null,
        };
        await invoke('start_vpn', { profile });
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
        set({ status: 'error' });
      } else {
        set({ status: mapVpnStatus(payload) });
      }
    });

    // Listen for real-time log entries from the Rust backend
    const unlistenLogs = await listen<LogEntry>('log-entry', (event) => {
      const entry = event.payload;
      get().addLog(entry.message, entry.level, entry.source);
    });

    return () => {
      unlisten();
      unlistenLogs();
    };
  },
}));
