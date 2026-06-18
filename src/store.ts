import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { ConnectionStatus, Server, Settings, Profile } from './types';

interface VpnStatusPayload {
  status: string | { error: string };
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

  setStatus: (status: ConnectionStatus) => void;
  setActiveServer: (server: Server) => void;
  setServers: (servers: Server[]) => void;
  addProfile: (profile: Omit<Profile, 'id'>) => void;
  updateProfile: (id: string, data: Partial<Profile>) => void;
  removeProfile: (id: string) => void;
  updateSettings: (settings: Partial<Settings>) => void;
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

export const useVPNStore = create<VPNStore>((set, get) => ({
  status: 'disconnected',
  activeServer: defaultServers[0],
  servers: defaultServers,
  profiles: [],
  hasOnboarded: false,
  hasAcceptedTerms: false,
  speeds: { up: 0, down: 0 },
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
  addProfile: (profileData) => {
    set((state) => ({
      profiles: [
        ...state.profiles,
        { ...profileData, id: crypto.randomUUID(), lastUpdated: Date.now() },
      ],
    }));
  },
  updateProfile: (id, data) => {
    set((state) => ({
      profiles: state.profiles.map((p) => (p.id === id ? { ...p, ...data } : p)),
    }));
  },
  removeProfile: (id) => {
    set((state) => ({
      profiles: state.profiles.filter((p) => p.id !== id),
    }));
  },
  updateSettings: (newSettings) => {
    set((state) => ({ settings: { ...state.settings, ...newSettings } }));
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
      const mapped = mapVpnStatus(rawStatus as VpnStatusPayload);
      set({ status: mapped });
    } catch {
      // Initial status fetch failed — stay disconnected
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
