/* eslint-disable @typescript-eslint/no-unsafe-return, @typescript-eslint/no-unsafe-member-access, @typescript-eslint/restrict-template-expressions */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { useAppStore, mapRustProfile } from './store';

// ─────────────────────────────────────────────────────────────────────────────
// Mock Tauri invoke for the store
// ─────────────────────────────────────────────────────────────────────────────
const mockInvoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn().mockImplementation((cmd: string, ...args: unknown[]) => {
    // log_frontend rejects to trigger addLog's dev fallback → appendLog
    if (cmd === 'log_frontend') return Promise.reject(new Error('mock'));
    return mockInvoke(cmd, ...args);
  }),
}));

// ─────────────────────────────────────────────────────────────────────────────
// Mock Tauri listen
// ─────────────────────────────────────────────────────────────────────────────
const mockListenFns: { event: string; handler: (payload: unknown) => void }[] = [];
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn((event: string, handler: (payload: unknown) => void) => {
    mockListenFns.push({ event, handler });
    return Promise.resolve(() => {
      const idx = mockListenFns.findIndex((e) => e.event === event);
      if (idx >= 0) mockListenFns.splice(idx, 1);
    });
  }),
}));

function triggerEvent(event: string, payload: unknown) {
  const entry = mockListenFns.find((e) => e.event === event);
  if (entry) entry.handler({ payload });
}

// ─────────────────────────────────────────────────────────────────────────────
// Types for test helpers
// ─────────────────────────────────────────────────────────────────────────────
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
  wg_local_address?: string;
  wg_allowed_ips?: string;
  wg_dns?: string;
  wg_mtu?: number;
  wg_keepalive?: number;
  username?: string;
  behind_cdn?: boolean;
  exit_flag?: string;
  exit_server?: string;
  exit_country_name?: string;
  exit_city_name?: string;
}

interface RustSubscriptionProfile {
  id: string;
  name: string;
  url: string;
  servers: RustServerProfile[];
  updated_at: string;
  traffic_used?: number;
  traffic_total?: number;
  traffic_expiry?: string;
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────
function makeRustProfile(
  overrides: Partial<RustSubscriptionProfile> = {},
): RustSubscriptionProfile {
  return {
    id: 'prof-1',
    name: 'Test Profile',
    url: 'https://example.com/sub',
    servers: [
      {
        id: 'srv-1',
        protocol: 'vless',
        server: '127.0.0.1',
        port: 443,
        country_name: 'Switzerland',
        exit_flag: 'CH',
        ping_ms: 50,
      },
      {
        id: 'srv-2',
        protocol: 'trojan',
        server: '192.168.1.1',
        port: 8443,
        country_name: 'Germany',
        exit_flag: 'DE',
        ping_ms: 100,
      },
    ],
    updated_at: '2026-06-26T00:00:00Z',
    ...overrides,
  };
}

beforeEach(() => {
  mockInvoke.mockReset();
  // Default: pick_replacement_server returns null (no preference — use first server)
  // This is consumed by getBestServer() in profile management tests.
  mockInvoke.mockResolvedValue(null);
  mockListenFns.length = 0;
  // Reset store to initial state
  useAppStore.setState({
    status: 'disconnected',
    activeServer: null,
    servers: [],
    profiles: [],
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
      autoTestInterval: 10,
      antiDpi: true,
      enableMux: false,
      minimizeToTray: false,
      blockCdnNodes: false,
      ipinfoToken: '',
      tlsInsecure: true,
      testUrl: 'cp.cloudflare.com/generate_204',
      testUseTls: true,
    },
  });
});

afterEach(() => {
  vi.clearAllTimers();
});

// ─────────────────────────────────────────────────────────────────────────────
// 1. Data mapping tests
// ─────────────────────────────────────────────────────────────────────────────
describe('mapRustProfile', () => {
  it('maps basic fields correctly', () => {
    const rust = makeRustProfile();
    const profile = mapRustProfile(rust);
    expect(profile.id).toBe('prof-1');
    expect(profile.name).toBe('Test Profile');
    expect(profile.url).toBe('https://example.com/sub');
    expect(profile.servers).toHaveLength(2);
  });

  it('maps country_name and city_name to display name', () => {
    const rust = makeRustProfile({
      servers: [
        {
          id: 'srv-1',
          protocol: 'vless',
          server: '10.0.0.1',
          port: 443,
          country_name: 'Japan',
          city_name: 'Tokyo',
          flag: 'JP',
        },
      ],
    });
    const profile = mapRustProfile(rust);
    expect(profile.servers[0].name).toBe('Japan, Tokyo');
    expect(profile.servers[0].countryName).toBe('Japan');
    expect(profile.servers[0].cityName).toBe('Tokyo');
    expect(profile.servers[0].flag).toBe('JP');
  });

  it('uses server:port when no country_name', () => {
    const rust = makeRustProfile({
      servers: [
        {
          id: 'srv-1',
          protocol: 'vless',
          server: '10.0.0.1',
          port: 443,
        },
      ],
    });
    const profile = mapRustProfile(rust);
    expect(profile.servers[0].name).toBe('10.0.0.1:443');
  });

  it('maps WireGuard fields', () => {
    const rust = makeRustProfile({
      servers: [
        {
          id: 'srv-wg',
          protocol: 'wireguard',
          server: '10.0.0.1',
          port: 51820,
          wg_local_address: '10.0.0.2/32',
          wg_allowed_ips: '0.0.0.0/0',
          wg_dns: '1.1.1.1',
          wg_mtu: 1280,
          wg_keepalive: 25,
          public_key: 'abc123',
          private_key: 'xyz789',
        },
      ],
    });
    const profile = mapRustProfile(rust);
    const s = profile.servers[0];
    expect(s.wgLocalAddress).toBe('10.0.0.2/32');
    expect(s.wgAllowedIps).toBe('0.0.0.0/0');
    expect(s.wgDns).toBe('1.1.1.1');
    expect(s.wgMtu).toBe(1280);
    expect(s.wgKeepalive).toBe(25);
    expect(s.publicKey).toBe('abc123');
    expect(s.privateKey).toBe('xyz789');
  });

  it('maps username field for SOCKS5/HTTP auth', () => {
    const rust = makeRustProfile({
      servers: [
        {
          id: 'srv-auth',
          protocol: 'socks',
          server: '10.0.0.1',
          port: 1080,
          username: 'proxyuser',
          password: 'proxypass',
          method: 'username_password',
        },
      ],
    });
    const profile = mapRustProfile(rust);
    expect(profile.servers[0].username).toBe('proxyuser');
    expect(profile.servers[0].password).toBe('proxypass');
    expect(profile.servers[0].method).toBe('username_password');
  });

  it('maps behind_cdn field', () => {
    const rust = makeRustProfile({
      servers: [
        {
          id: 'srv-cdn',
          protocol: 'vless',
          server: 'cdn.example.com',
          port: 443,
          behind_cdn: true,
        },
      ],
    });
    const profile = mapRustProfile(rust);
    expect(profile.servers[0].behindCdn).toBe(true);
  });

  it('handles undefined optional fields gracefully', () => {
    const rust = makeRustProfile({
      servers: [
        {
          id: 'srv-min',
          protocol: 'vless',
          server: '10.0.0.1',
          port: 443,
        },
      ],
    });
    const profile = mapRustProfile(rust);
    const s = profile.servers[0];
    expect(s.uuid).toBeUndefined();
    expect(s.password).toBeUndefined();
    expect(s.wgLocalAddress).toBeUndefined();
    expect(s.username).toBeUndefined();
    expect(s.behindCdn).toBeUndefined();
  });

  it('parses updated_at timestamp fallback', () => {
    const rust = makeRustProfile({ updated_at: 'invalid-date' });
    const profile = mapRustProfile(rust);
    expect(typeof profile.lastUpdated).toBe('number');
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 2. Store actions — basic state
// ─────────────────────────────────────────────────────────────────────────────
describe('store actions — basic state', () => {
  it('starts in disconnected state', () => {
    const state = useAppStore.getState();
    expect(state.status).toBe('disconnected');
    expect(state.activeServer).toBeNull();
    expect(state.servers).toEqual([]);
    expect(state.profiles).toEqual([]);
  });

  it('setStatus updates status', () => {
    useAppStore.getState().setStatus('connecting');
    expect(useAppStore.getState().status).toBe('connecting');
  });

  it('setActiveServer updates active server', () => {
    const srv = { id: 'srv-1', name: 'Test', server: '10.0.0.1', port: 443 };
    useAppStore.getState().setActiveServer(srv);
    expect(useAppStore.getState().activeServer?.id).toBe('srv-1');
    // (addLog removed — system-level logging cleaned)
  });

  it('setConnectedAt stores timestamp', () => {
    const ts = 1234567890;
    useAppStore.getState().setConnectedAt(ts);
    expect(useAppStore.getState().connectedAt).toBe(ts);
  });

  it('clearLogs empties the array', async () => {
    useAppStore.getState().addLog('test log');
    await new Promise((r) => setTimeout(r, 300)); // flush log buffer
    expect(useAppStore.getState().logs.length).toBe(1);
    useAppStore.getState().clearLogs();
    expect(useAppStore.getState().logs).toEqual([]);
  });

  it('addLog prepends entry and caps at 1000', async () => {
    for (let i = 0; i < 1010; i++) {
      useAppStore.getState().addLog(`msg-${i}`);
    }
    await new Promise((r) => setTimeout(r, 300)); // flush log buffer
    expect(useAppStore.getState().logs.length).toBe(1000);
    expect(useAppStore.getState().logs[0].message).toContain('msg-1009');
  });

  it('completeOnboarding sets hasOnboarded true', () => {
    expect(useAppStore.getState().hasOnboarded).toBe(false);
    useAppStore.getState().completeOnboarding();
    expect(useAppStore.getState().hasOnboarded).toBe(true);
  });

  it('acceptTerms sets hasAcceptedTerms true', () => {
    expect(useAppStore.getState().hasAcceptedTerms).toBe(false);
    useAppStore.getState().acceptTerms();
    expect(useAppStore.getState().hasAcceptedTerms).toBe(true);
  });

  it('setSpeeds stores current speed data', () => {
    useAppStore.getState().setSpeeds({ up: 1024, down: 2048 });
    expect(useAppStore.getState().speeds).toEqual({ up: 1024, down: 2048 });
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 3. Profile management
// ─────────────────────────────────────────────────────────────────────────────
describe('store — profile management', () => {
  it('addProfile calls invoke and adds servers', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useAppStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' });
    expect(mockInvoke).toHaveBeenCalledWith('add_subscription', {
      name: 'Test',
      url: 'https://example.com/sub',
      profileId: expect.any(String),
    });
    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.servers).toHaveLength(2);
    expect(state.activeServer).not.toBeNull();
  });

  it('addProfile sets activeServer from first server', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useAppStore.getState().addProfile({ name: 'Test', url: 'https://...' });
    expect(useAppStore.getState().activeServer?.id).toBe('srv-1');
  });

  it('addProfile rethrows on failure', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('network error'));
    await expect(
      useAppStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' }),
    ).rejects.toThrow('network error');
    // Backend handles error logging via tracing — frontend just rethrows
  });

  it('removeProfile removes profile and servers', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useAppStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' });
    mockInvoke.mockResolvedValueOnce(undefined);
    await useAppStore.getState().removeProfile('prof-1');
    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(0);
    expect(state.servers).toHaveLength(0);
    expect(state.activeServer).toBeFalsy();
  });

  it('removeProfile preserves activeServer when not in removed profile', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useAppStore.getState().addProfile({ name: 'First', url: 'https://example.com/1' });
    expect(useAppStore.getState().activeServer?.id).toBe('srv-1');

    mockInvoke.mockResolvedValueOnce(
      makeRustProfile({
        id: 'prof-2',
        servers: [{ id: 'srv-3', protocol: 'vless', server: '10.0.0.3', port: 443 }],
      }),
    );
    await useAppStore.getState().addProfile({ name: 'Second', url: 'https://example.com/2' });

    useAppStore
      .getState()
      .setActiveServer({ id: 'srv-1', name: 'Srv1', server: '10.0.0.1', port: 443 });

    mockInvoke.mockResolvedValueOnce(undefined);
    await useAppStore.getState().removeProfile('prof-2');
    expect(useAppStore.getState().activeServer?.id).toBe('srv-1');
  });

  it('refreshProfile updates profile and servers preserving manual selection', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useAppStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' });
    expect(useAppStore.getState().activeServer?.id).toBe('srv-1');

    // Disable autoConnectFastest so manual selection is preserved
    useAppStore.getState().updateSettings({ autoConnectFastest: false });

    // Refresh with new ID but same data
    const refreshed = makeRustProfile({
      servers: [
        {
          id: 'srv-1-new',
          protocol: 'vless',
          server: '127.0.0.1',
          port: 443,
          country_name: 'Switzerland',
          flag: 'CH',
          ping_ms: 50,
        },
      ],
    });
    mockInvoke.mockResolvedValueOnce(refreshed);
    await useAppStore.getState().refreshProfile('prof-1');

    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.profiles[0].servers).toHaveLength(1);
    // activeServer falls to first available when current is removed (even with autoConnectFastest=false)
    expect(state.activeServer?.id).toBe('srv-1-new');
  });

  it('refreshProfile with autoConnectFastest selects fastest server', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useAppStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' });
    expect(useAppStore.getState().activeServer?.id).toBe('srv-1');

    // autoConnectFastest is true by default
    const refreshed = makeRustProfile({
      servers: [
        { id: 'srv-new-1', protocol: 'vless', server: '10.0.0.1', port: 443, ping_ms: 200 },
        { id: 'srv-new-2', protocol: 'vless', server: '10.0.0.2', port: 443, ping_ms: 30 },
      ],
    });
    mockInvoke.mockResolvedValueOnce(refreshed);
    // getBestServer now operates on the post-refresh server list (srv-new-1, srv-new-2)
    // pick_replacement_server returns the lowest-ping server from the backend
    mockInvoke.mockResolvedValueOnce({ id: 'srv-new-2' });
    await useAppStore.getState().refreshProfile('prof-1');

    const state = useAppStore.getState();
    // Fastest ping is 30 → srv-new-2
    expect(state.activeServer?.id).toBe('srv-new-2');
  });

  it('refreshProfile rethrows on failure', async () => {
    useAppStore.getState().updateSettings({ autoConnectFastest: false });
    mockInvoke.mockRejectedValueOnce(new Error('timeout'));
    await expect(useAppStore.getState().refreshProfile('prof-1')).rejects.toThrow('timeout');
    // Backend handles error logging via tracing — frontend just rethrows
  });

  it('loadProfiles loads and selects fastest on first run', async () => {
    mockInvoke.mockResolvedValueOnce([makeRustProfile()]);
    await useAppStore.getState().loadProfiles();
    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.servers).toHaveLength(2);
    // On first load: selects fastest (srv-1 ping 50)
    expect(state.activeServer?.id).toBe('srv-1');
  });

  it('loadProfiles always selects fastest server', async () => {
    mockInvoke.mockResolvedValueOnce([makeRustProfile()]);
    await useAppStore.getState().loadProfiles();
    expect(useAppStore.getState().activeServer?.id).toBe('srv-1');

    // Set manual server
    useAppStore
      .getState()
      .setActiveServer({ id: 'srv-2', name: 'Manual', server: '10.0.0.2', port: 8080 });

    mockInvoke.mockResolvedValueOnce([makeRustProfile()]);
    await useAppStore.getState().loadProfiles();
    // Always picks fastest — srv-1 (ping 50) over srv-2 (ping 100)
    expect(useAppStore.getState().activeServer?.id).toBe('srv-1');
  });

  it('loadProfiles handles invoke failure gracefully', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('connection error'));
    await useAppStore.getState().loadProfiles();
    // Backend handles error logging via tracing — frontend silently fails
  });

  it('updateProfile merges partial data', () => {
    useAppStore.setState({
      profiles: [{ id: 'prof-1', name: 'Original', url: 'https://x.com', servers: [] }],
    });
    useAppStore.getState().updateProfile('prof-1', { name: 'Renamed' });
    expect(useAppStore.getState().profiles[0].name).toBe('Renamed');
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 4. Settings management
// ─────────────────────────────────────────────────────────────────────────────
describe('store — settings management', () => {
  it('updateSettings merges and bumps version', () => {
    const v0 = useAppStore.getState().settingsVersion;
    useAppStore.getState().updateSettings({ routingMode: 'local-proxy', proxyPort: 9090 });
    const state = useAppStore.getState();
    expect(state.settings.routingMode).toBe('local-proxy');
    expect(state.settings.proxyPort).toBe(9090);
    expect(state.settingsVersion).toBe(v0 + 1);
  });

  it('saveSettings invokes update_settings', async () => {
    mockInvoke.mockResolvedValueOnce(undefined);
    await useAppStore.getState().saveSettings();
    expect(mockInvoke).toHaveBeenCalledWith('update_settings', {
      newSettings: useAppStore.getState().settings,
    });
  });

  it('saveSettings handles failure gracefully', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('save failed'));
    await useAppStore.getState().saveSettings();
    // Backend handles error logging via tracing — frontend silently fails
  });

  it('loadSettings stores backend-normalised routing mode', async () => {
    mockInvoke.mockResolvedValueOnce({ routingMode: 'system-proxy' });
    await useAppStore.getState().loadSettings();
    expect(useAppStore.getState().settings.routingMode).toBe('system-proxy');
  });

  it('loadSettings handles failure gracefully', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('fail'));
    await useAppStore.getState().loadSettings();
    // Backend handles error logging via tracing — frontend silently fails
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 5. Proxy connection toggling
// ─────────────────────────────────────────────────────────────────────────────
describe('store — toggleConnection', () => {
  it('disconnect path calls stop_proxy', async () => {
    useAppStore.setState({ status: 'connected' });
    mockInvoke.mockResolvedValueOnce(undefined);
    await useAppStore.getState().toggleConnection();
    expect(mockInvoke).toHaveBeenCalledWith('stop_proxy');
    expect(useAppStore.getState().status).toBe('disconnecting');
  });

  it('connect path calls saveSettings then start_proxy', async () => {
    useAppStore.setState({
      activeServer: { id: 'srv-1', name: 'Test', protocol: 'vless', server: '10.0.0.1', port: 443 },
      servers: [
        { id: 'srv-1', name: 'Test', protocol: 'vless', server: '10.0.0.1', port: 443 },
        { id: 'srv-2', name: 'Alt', protocol: 'vless', server: '10.0.0.2', port: 443 },
      ],
      realIp: { server: '1.2.3.4', flag: '', country_name: '', city_name: '' },
    });
    mockInvoke.mockResolvedValueOnce(undefined); // saveSettings — update_settings
    mockInvoke.mockResolvedValueOnce(1080); // start_proxy — returns actual port
    mockInvoke.mockResolvedValueOnce({
      server: '5.6.7.8',
      flag: 'PL',
      country_name: 'Poland',
      city_name: 'Warsaw',
    }); // outlet_ip

    await useAppStore.getState().toggleConnection();

    expect(mockInvoke).toHaveBeenCalledWith(
      'start_proxy',
      expect.objectContaining({
        routingMode: 'system-proxy',
        proxyPort: 1080,
      }),
    );
    expect(useAppStore.getState().status).toBe('connecting');
  });

  it('connect path populates alternativeServers when autoConnectFastest', async () => {
    const activeServer = {
      id: 'srv-1',
      name: 'Main',
      protocol: 'vless',
      server: '10.0.0.1',
      port: 443,
    } as never;
    useAppStore.setState({
      activeServer,
      servers: [
        { id: 'srv-1', name: 'Main', server: '10.0.0.1', port: 443 },
        { id: 'srv-2', name: 'Alt1', server: '10.0.0.2', port: 443 },
        { id: 'srv-3', name: 'Alt2', server: '10.0.0.3', port: 443 },
      ],
      realIp: { server: '1.2.3.4', flag: '', country_name: '', city_name: '' },
    });
    mockInvoke.mockResolvedValueOnce(undefined); // saveSettings — update_settings
    mockInvoke.mockResolvedValueOnce(1080); // start_proxy — returns actual port
    mockInvoke.mockResolvedValueOnce({
      server: '5.6.7.8',
      flag: 'PL',
      country_name: 'Poland',
      city_name: 'Warsaw',
    }); // outlet_ip

    await useAppStore.getState().toggleConnection();

    const startCall = mockInvoke.mock.calls.find((c) => c[0] === 'start_proxy');
    expect(startCall).toBeDefined();
    // alternativeServers should have 2 entries (all except active)
    expect(startCall![1].alternativeServers).toHaveLength(2);
  });

  it('connect path does NOT populate altServers when autoConnectFastest disabled', async () => {
    useAppStore.setState({
      activeServer: { id: 'srv-1', name: 'Test', protocol: 'vless', server: '10.0.0.1', port: 443 },
      servers: [{ id: 'srv-1', name: 'Test', server: '10.0.0.1', port: 443 }],
      realIp: { server: '1.2.3.4', flag: '', country_name: '', city_name: '' },
    });
    useAppStore.getState().updateSettings({ autoConnectFastest: false });
    mockInvoke.mockResolvedValueOnce(undefined); // saveSettings — update_settings
    mockInvoke.mockResolvedValueOnce(1080); // start_proxy — returns actual port
    mockInvoke.mockResolvedValueOnce({
      server: '5.6.7.8',
      flag: 'PL',
      country_name: 'Poland',
      city_name: 'Warsaw',
    }); // outlet_ip

    await useAppStore.getState().toggleConnection();
    const startCall = mockInvoke.mock.calls.find((c) => c[0] === 'start_proxy');
    expect(startCall).toBeDefined();
    expect(startCall![1].alternativeServers).toHaveLength(0);
  });

  it('guards against reconnect during toggle', async () => {
    useAppStore.setState({ _isReconnecting: true });
    await useAppStore.getState().toggleConnection();
    // Should not call invoke at all (silent guard — no log)
    expect(mockInvoke).not.toHaveBeenCalledWith('start_proxy');
    expect(mockInvoke).not.toHaveBeenCalledWith('stop_proxy');
  });

  it('sets error state on connection failure', async () => {
    useAppStore.setState({
      activeServer: { id: 'srv-1', name: 'Test', server: '10.0.0.1', port: 443 },
      realIp: { server: '1.2.3.4', flag: '', country_name: '', city_name: '' },
    });
    mockInvoke.mockResolvedValueOnce(undefined); // saveSettings — update_settings
    mockInvoke.mockRejectedValueOnce(new Error('timeout')); // start_proxy fails

    // Frontend swallows errors & shows toast — does not re-throw
    await useAppStore.getState().toggleConnection();
    expect(useAppStore.getState().status).toBe('error');
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 6. IP fetching
// ─────────────────────────────────────────────────────────────────────────────
describe('store — IP fetching', () => {
  it('fetchOutletIp stores IP on success', async () => {
    mockInvoke.mockResolvedValueOnce({
      server: '1.2.3.4',
      flag: 'PL',
      country_name: 'Poland',
      city_name: 'Warsaw',
    });
    await useAppStore.getState().fetchExitIp();
    const result = useAppStore.getState().exitIp;
    expect(result).not.toBeNull();
    expect(result?.server).toBe('1.2.3.4');
  });

  it('fetchOutletIp stores null on error', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('fail'));
    await useAppStore.getState().fetchExitIp();
    expect(useAppStore.getState().exitIp).toBeNull();
  });

  it('fetchRealIp stores IP on success', async () => {
    mockInvoke.mockResolvedValueOnce({
      server: '9.9.9.9',
      flag: '',
      country_name: '',
      city_name: '',
    });
    await useAppStore.getState().fetchRealIp();
    expect(useAppStore.getState().realIp?.server).toBe('9.9.9.9');
  });

  it('fetchRealIp stores null on error', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('fail'));
    await useAppStore.getState().fetchRealIp();
    expect(useAppStore.getState().realIp).toBeNull();
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 7. Speed test
// ─────────────────────────────────────────────────────────────────────────────
describe('store — speed test', () => {
  it('runSpeedTest stores the fastest result', async () => {
    mockInvoke.mockResolvedValueOnce([
      { delay: 100, mean_delay: 100 },
      { delay: 50, mean_delay: 50 },
      { delay: 200, mean_delay: 200 },
    ]);
    const result = await useAppStore.getState().runSpeedTest();
    expect(result?.delay).toBe(50);
    expect(useAppStore.getState().speedTestResult?.delay).toBe(50);
  });

  it('runSpeedTest returns null on error', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('fail'));
    const result = await useAppStore.getState().runSpeedTest();
    expect(result).toBeNull();
    expect(useAppStore.getState().speedTestResult).toBeNull();
  });

  it('runSpeedTest returns null when no results', async () => {
    mockInvoke.mockResolvedValueOnce([]);
    const result = await useAppStore.getState().runSpeedTest();
    expect(result).toBeNull();
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 8. Health check / reconnect
// ─────────────────────────────────────────────────────────────────────────────
describe('store — initListener health check', () => {
  beforeEach(function () {
    mockInvoke.mockResolvedValue(undefined);
    useAppStore.setState({
      activeServer: { id: 'srv-1', name: 'Test', flag: 'CH', server: '10.0.0.1', port: 443 },
      servers: [
        { id: 'srv-1', name: 'Srv1', flag: 'CH', server: '10.0.0.1', port: 443, ping: 50 },
        { id: 'srv-2', name: 'Srv2', flag: 'DE', server: '10.0.0.2', port: 443, ping: 100 },
      ],
    });
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('registers connection-status-changed listener', async () => {
    const cleanup = await useAppStore.getState().initListener();
    expect(mockListenFns.some((e) => e.event === 'connection-status-changed')).toBe(true);
    cleanup();
  });

  it('registers log-entry, traffic-update and refresh-progress listeners', async () => {
    const cleanup = await useAppStore.getState().initListener();
    expect(mockListenFns.some((e) => e.event === 'log-entry')).toBe(true);
    expect(mockListenFns.some((e) => e.event === 'traffic-update')).toBe(true);
    expect(mockListenFns.some((e) => e.event === 'refresh-progress')).toBe(true);
    cleanup();
  });

  it('connection-status-changed connected sets status and timestamp', async () => {
    const cleanup = await useAppStore.getState().initListener();
    triggerEvent('connection-status-changed', { status: 'connected' });
    expect(useAppStore.getState().status).toBe('connected');
    expect(useAppStore.getState().connectedAt).not.toBeNull();
    cleanup();
  });

  it('connection-status-changed disconnected clears timestamp', async () => {
    const cleanup = await useAppStore.getState().initListener();
    triggerEvent('connection-status-changed', { status: 'connected' });
    triggerEvent('connection-status-changed', { status: 'disconnected' });
    expect(useAppStore.getState().status).toBe('disconnected');
    expect(useAppStore.getState().connectedAt).toBeNull();
    cleanup();
  });

  it('connection-status-changed error sets error status', async () => {
    const cleanup = await useAppStore.getState().initListener();
    triggerEvent('connection-status-changed', { status: { error: 'lost' } });
    expect(useAppStore.getState().status).toBe('error');
    cleanup();
  });

  it('traffic-update updates speeds', async () => {
    const cleanup = await useAppStore.getState().initListener();
    triggerEvent('traffic-update', { up: 12345, down: 67890 });
    expect(useAppStore.getState().speeds).toEqual({ up: 12345, down: 67890 });
    cleanup();
  });

  it.skip('log-entry event adds to log buffer', async () => {
    // get_logs and get_status must return clean values so initListener doesn't log errors
    mockInvoke.mockResolvedValueOnce('disconnected'); // get_status
    mockInvoke.mockResolvedValueOnce([]); // get_logs
    const cleanup = await useAppStore.getState().initListener();
    expect(useAppStore.getState().logs.length).toBe(0);
    triggerEvent('log-entry', {
      message: 'from rust',
      level: 'info',
      source: 'system',
      id: 'abc',
      timestamp: Date.now(),
    });
    await new Promise((r) => setTimeout(r, 400)); // flush log buffer
    expect(useAppStore.getState().logs.length).toBe(1);
    expect(useAppStore.getState().logs[0].message).toBe('from rust');
    cleanup();
  });

  it('cleanup function unlistens all events', async () => {
    mockInvoke.mockResolvedValueOnce('disconnected'); // get_status
    mockInvoke.mockResolvedValueOnce([]); // get_logs
    const cleanup = await useAppStore.getState().initListener();

    // After cleanup, events should not be processed
    cleanup();
    triggerEvent('connection-status-changed', { status: 'connected' });
    expect(useAppStore.getState().status).toBe('disconnected');
  });

  it('failover-occurred event switches active server', async () => {
    mockInvoke.mockResolvedValueOnce('disconnected');
    mockInvoke.mockResolvedValueOnce([]);
    const cleanup = await useAppStore.getState().initListener();
    useAppStore.setState({
      servers: [
        { id: 'srv-1', name: 'Primary', server: '10.0.0.1', port: 443 },
        { id: 'srv-2', name: 'Backup', server: '10.0.0.2', port: 443 },
      ],
      activeServer: { id: 'srv-1', name: 'Primary', server: '10.0.0.1', port: 443 },
    });

    triggerEvent('failover-occurred', 'srv-2');
    expect(useAppStore.getState().activeServer?.id).toBe('srv-2');
    cleanup();
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 10. refreshConnections
// ─────────────────────────────────────────────────────────────────────────────
describe('store — connections', () => {
  it('refreshConnections stores connection list', async () => {
    mockInvoke.mockResolvedValueOnce([{ id: 'conn-1', host: 'example.com' }]);
    await useAppStore.getState().refreshConnections();
    expect(useAppStore.getState().connections).toHaveLength(1);
    expect(useAppStore.getState().connections[0].host).toBe('example.com');
  });

  it('refreshConnections handles error gracefully', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('not connected'));
    await useAppStore.getState().refreshConnections();
    expect(useAppStore.getState().connections).toEqual([]);
  });
});
