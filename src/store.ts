import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { ConnectionStatus, Profile, Server, Settings } from './types';

interface VpnStatusPayload {
  status: string | { error: string };
}

interface RustServerProfile {
  id: string;
  name: string;
  protocol: string;
  server: string;
  port: number;
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
  isDirty: boolean;
  savingIn: number | null;

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
  completeOnboarding: () => void;
  acceptTerms: () => void;
  setSpeeds: (speeds: { up: number; down: number }) => void;
  toggleConnection: () => Promise<void>;
  initListener: () => Promise<() => void>;
}

const defaultServers: Server[] = [
  { id: '1', name: 'Frankfurt', flag: '🇩🇪', ping: 45 },
  { id: '2', name: 'Amsterdam', flag: '🇳🇱', ping: 52 },
  { id: '3', name: 'London', flag: '🇬🇧', ping: 68 },
  { id: '4', name: 'New York', flag: '🇺🇸', ping: 120 },
];

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
        host: s.server,
        port: s.port,
      }),
    ),
  };
}

let _debounceTimer: ReturnType<typeof setTimeout> | null = null;
let _countdownInterval: ReturnType<typeof setInterval> | null = null;

export const useVPNStore = create<VPNStore>((set, get) => ({
  status: 'disconnected',
  activeServer: defaultServers[0],
  servers: defaultServers,
  profiles: [],
  hasOnboarded: false,
  hasAcceptedTerms: false,
  speeds: { up: 0, down: 0 },
  isDirty: false,
  savingIn: null,
  settings: {
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
      set((state) => ({ profiles: [...state.profiles, profile] }));
    } catch (e) {
      // Error handled by caller (toast)
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
      set((state) => ({ profiles: state.profiles.filter((p) => p.id !== id) }));
    } catch (e) {
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
    } catch (e) {
      throw e;
    }
  },

  loadProfiles: async () => {
    try {
      const results = await invoke<RustSubscriptionProfile[]>('get_subscriptions');
      set({ profiles: results.map(mapRustProfile) });
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      console.warn('[omnitun] Failed to load profiles:', msg);
    }
  },

  updateSettings: (newSettings) => {
    set((state) => ({
      settings: { ...state.settings, ...newSettings },
      isDirty: true,
      savingIn: 2.0,
    }));

    if (_debounceTimer) clearTimeout(_debounceTimer);
    if (_countdownInterval) clearInterval(_countdownInterval);

    const startTime = Date.now();
    const duration = 2000;
    _countdownInterval = setInterval(() => {
      const elapsed = Date.now() - startTime;
      const remaining = Math.max(0, (duration - elapsed) / 1000);
      set({ savingIn: remaining });
      if (remaining <= 0) {
        if (_countdownInterval) {
          clearInterval(_countdownInterval);
          _countdownInterval = null;
        }
      }
    }, 100);

    _debounceTimer = setTimeout(() => {
      get().saveSettings();
    }, duration);
  },

  saveSettings: async () => {
    try {
      const { settings } = get();
      await invoke('update_settings', { newSettings: settings });
      set({ isDirty: false, savingIn: null });
    } catch (e) {
      console.error('[omnitun] Failed to save settings:', e);
    }
  },

  loadSettings: async () => {
    try {
      const result = await invoke<Settings>('get_settings');
      set({ settings: result, isDirty: false, savingIn: null });
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      console.warn('[omnitun] Failed to load settings:', msg);
    }
  },
  completeOnboarding: () => {
    set({ hasOnboarded: true });
  },
  acceptTerms: () => {
    set({ hasAcceptedTerms: true });
  },
  setSpeeds: (speeds) => {
    set({ speeds });
  },

  toggleConnection: async () => {
    const { status } = get();
    try {
      if (status === 'disconnected' || status === 'error') {
        set({ status: 'connecting' });
        const profile = {
          id: 'default',
          name: get().activeServer?.name ?? 'Default',
          protocol: 'vless',
          server: '127.0.0.1',
          port: 443,
        };
        await invoke('start_vpn', { profile });
      } else {
        await invoke('stop_vpn');
        set({ status: 'disconnected' });
      }
    } catch (e: unknown) {
      set({ status: 'error' });
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
      console.warn('[omnitun] Failed to fetch initial VPN status:', msg);
      // Stay disconnected — it's the default initial state
      set({ status: 'disconnected' });
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

    return unlisten;
  },
}));
