/* eslint-disable @typescript-eslint/no-unsafe-return, @typescript-eslint/no-unsafe-member-access, @typescript-eslint/restrict-template-expressions */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { useVPNStore, mapRustProfile, pickReplacementServer } from './store';

// ─────────────────────────────────────────────────────────────────────────────
// Mock Tauri invoke for the store
// ─────────────────────────────────────────────────────────────────────────────
const mockInvoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

// ─────────────────────────────────────────────────────────────────────────────
// Helper: build a minimal get_subscriptions mock response from servers array
// ─────────────────────────────────────────────────────────────────────────────
function mockGetSubscriptions(servers: unknown[]): unknown[] {
  return [{
    id: 'prof-mock',
    name: 'Mock',
    url: '',
    servers: servers.map((s: Record<string, unknown>) => ({
      id: s.id,
      protocol: s.protocol ?? 'vless',
      server: s.server ?? '127.0.0.1',
      port: s.port ?? 0,
    })),
    updated_at: new Date().toISOString(),
  }];
}

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
        flag: 'CH',
        ping_ms: 50,
      },
      {
        id: 'srv-2',
        protocol: 'trojan',
        server: '192.168.1.1',
        port: 8443,
        country_name: 'Germany',
        flag: 'DE',
        ping_ms: 100,
      },
    ],
    updated_at: '2026-06-26T00:00:00Z',
    ...overrides,
  };
}

beforeEach(() => {
  mockInvoke.mockReset();
  mockListenFns.length = 0;
  // Reset store to initial state
  useVPNStore.setState({
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
    _healthCheckTimer: null,
    _isReconnecting: false,
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
      blockCdnNodes: false,
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
    const state = useVPNStore.getState();
    expect(state.status).toBe('disconnected');
    expect(state.activeServer).toBeNull();
    expect(state.servers).toEqual([]);
    expect(state.profiles).toEqual([]);
  });

  it('setStatus updates status', () => {
    useVPNStore.getState().setStatus('connecting');
    expect(useVPNStore.getState().status).toBe('connecting');
  });

  it('setActiveServer updates and logs change', () => {
    const srv = { id: 'srv-1', name: 'Test', server: '10.0.0.1', port: 443 };
    useVPNStore.getState().setActiveServer(srv);
    expect(useVPNStore.getState().activeServer?.id).toBe('srv-1');
    expect(useVPNStore.getState().logs.length).toBeGreaterThan(0);
    expect(useVPNStore.getState().logs[0].message).toContain('activeServer');
  });

  it('setConnectedAt stores timestamp', () => {
    const ts = 1234567890;
    useVPNStore.getState().setConnectedAt(ts);
    expect(useVPNStore.getState().connectedAt).toBe(ts);
  });

  it('clearLogs empties the array', () => {
    useVPNStore.getState().addLog('test log');
    expect(useVPNStore.getState().logs.length).toBe(1);
    useVPNStore.getState().clearLogs();
    expect(useVPNStore.getState().logs).toEqual([]);
  });

  it('addLog prepends entry and caps at 1000', () => {
    for (let i = 0; i < 1010; i++) {
      useVPNStore.getState().addLog(`msg-${i}`);
    }
    expect(useVPNStore.getState().logs.length).toBe(1000);
    expect(useVPNStore.getState().logs[0].message).toContain('msg-1009');
  });

  it('completeOnboarding sets hasOnboarded true', () => {
    expect(useVPNStore.getState().hasOnboarded).toBe(false);
    useVPNStore.getState().completeOnboarding();
    expect(useVPNStore.getState().hasOnboarded).toBe(true);
  });

  it('acceptTerms sets hasAcceptedTerms true', () => {
    expect(useVPNStore.getState().hasAcceptedTerms).toBe(false);
    useVPNStore.getState().acceptTerms();
    expect(useVPNStore.getState().hasAcceptedTerms).toBe(true);
  });

  it('setSpeeds stores current speed data', () => {
    useVPNStore.getState().setSpeeds({ up: 1024, down: 2048 });
    expect(useVPNStore.getState().speeds).toEqual({ up: 1024, down: 2048 });
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 3. Profile management
// ─────────────────────────────────────────────────────────────────────────────
describe('store — profile management', () => {
  it('addProfile calls invoke and adds servers', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useVPNStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' });
    expect(mockInvoke).toHaveBeenCalledWith('add_subscription', {
      name: 'Test',
      url: 'https://example.com/sub',
    });
    const state = useVPNStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.servers).toHaveLength(2);
    expect(state.activeServer).not.toBeNull();
  });

  it('addProfile sets activeServer from first server', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useVPNStore.getState().addProfile({ name: 'Test', url: 'https://...' });
    expect(useVPNStore.getState().activeServer?.id).toBe('srv-1');
  });

  it('addProfile logs error on failure', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('network error'));
    await expect(
      useVPNStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' }),
    ).rejects.toThrow('network error');
    expect(useVPNStore.getState().logs.some((l) => l.message.includes('Failed to add'))).toBe(true);
  });

  it('removeProfile removes profile and servers', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useVPNStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' });
    mockInvoke.mockResolvedValueOnce(undefined);
    await useVPNStore.getState().removeProfile('prof-1');
    const state = useVPNStore.getState();
    expect(state.profiles).toHaveLength(0);
    expect(state.servers).toHaveLength(0);
    expect(state.activeServer).toBeFalsy();
  });

  it('removeProfile preserves activeServer when not in removed profile', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useVPNStore.getState().addProfile({ name: 'First', url: 'https://example.com/1' });
    expect(useVPNStore.getState().activeServer?.id).toBe('srv-1');

    mockInvoke.mockResolvedValueOnce(
      makeRustProfile({
        id: 'prof-2',
        servers: [{ id: 'srv-3', protocol: 'vless', server: '10.0.0.3', port: 443 }],
      }),
    );
    await useVPNStore.getState().addProfile({ name: 'Second', url: 'https://example.com/2' });

    useVPNStore
      .getState()
      .setActiveServer({ id: 'srv-1', name: 'Srv1', server: '10.0.0.1', port: 443 });

    mockInvoke.mockResolvedValueOnce(undefined);
    await useVPNStore.getState().removeProfile('prof-2');
    expect(useVPNStore.getState().activeServer?.id).toBe('srv-1');
  });

  it('refreshProfile updates profile and servers preserving manual selection', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useVPNStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' });
    expect(useVPNStore.getState().activeServer?.id).toBe('srv-1');

    // Disable autoConnectFastest so manual selection is preserved
    useVPNStore.getState().updateSettings({ autoConnectFastest: false });

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
    await useVPNStore.getState().refreshProfile('prof-1');

    const state = useVPNStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.profiles[0].servers).toHaveLength(1);
    // activeServer stays at old one because autoConnectFastest=false
    expect(state.activeServer?.id).toBe('srv-1');
  });

  it('refreshProfile with autoConnectFastest selects fastest server', async () => {
    mockInvoke.mockResolvedValueOnce(makeRustProfile());
    await useVPNStore.getState().addProfile({ name: 'Test', url: 'https://example.com/sub' });
    expect(useVPNStore.getState().activeServer?.id).toBe('srv-1');

    // autoConnectFastest is true by default
    const refreshed = makeRustProfile({
      servers: [
        { id: 'srv-new-1', protocol: 'vless', server: '10.0.0.1', port: 443, ping_ms: 200 },
        { id: 'srv-new-2', protocol: 'vless', server: '10.0.0.2', port: 443, ping_ms: 30 },
      ],
    });
    mockInvoke.mockResolvedValueOnce(refreshed);
    await useVPNStore.getState().refreshProfile('prof-1');

    const state = useVPNStore.getState();
    // Fastest ping is 30 -> srv-new-2
    expect(state.activeServer?.id).toBe('srv-new-2');
  });

  it('refreshProfile logs error on failure', async () => {
    useVPNStore.getState().updateSettings({ autoConnectFastest: false });
    mockInvoke.mockRejectedValueOnce(new Error('timeout'));
    await expect(useVPNStore.getState().refreshProfile('prof-1')).rejects.toThrow('timeout');
    expect(useVPNStore.getState().logs.some((l) => l.message.includes('Failed to refresh'))).toBe(
      true,
    );
  });

  it('loadProfiles loads and selects fastest on first run', async () => {
    mockInvoke.mockResolvedValueOnce([makeRustProfile()]);
    await useVPNStore.getState().loadProfiles();
    const state = useVPNStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.servers).toHaveLength(2);
    // On first load: selects fastest (srv-1 ping 50)
    expect(state.activeServer?.id).toBe('srv-1');
  });

  it('loadProfiles always selects fastest server', async () => {
    mockInvoke.mockResolvedValueOnce([makeRustProfile()]);
    await useVPNStore.getState().loadProfiles();
    expect(useVPNStore.getState().activeServer?.id).toBe('srv-1');

    // Set manual server
    useVPNStore.getState().setActiveServer({ id: 'srv-2', name: 'Manual', server: '10.0.0.2', port: 8080 });

    mockInvoke.mockResolvedValueOnce([makeRustProfile()]);
    await useVPNStore.getState().loadProfiles();
    // Always picks fastest — srv-1 (ping 50) over srv-2 (ping 100)
    expect(useVPNStore.getState().activeServer?.id).toBe('srv-1');
  });

  it('loadProfiles handles invoke failure gracefully', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('connection error'));
    await useVPNStore.getState().loadProfiles();
    // Should not throw, should log warning
    expect(useVPNStore.getState().logs.some((l) => l.message.includes('Failed to load'))).toBe(
      true,
    );
  });

  it('updateProfile merges partial data', () => {
    useVPNStore.setState({
      profiles: [{ id: 'prof-1', name: 'Original', url: 'https://x.com', servers: [] }],
    });
    useVPNStore.getState().updateProfile('prof-1', { name: 'Renamed' });
    expect(useVPNStore.getState().profiles[0].name).toBe('Renamed');
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 4. Settings management
// ─────────────────────────────────────────────────────────────────────────────
describe('store — settings management', () => {
  it('updateSettings merges and bumps version', () => {
    const v0 = useVPNStore.getState().settingsVersion;
    useVPNStore.getState().updateSettings({ routingMode: 'local-proxy', proxyPort: 9090 });
    const state = useVPNStore.getState();
    expect(state.settings.routingMode).toBe('local-proxy');
    expect(state.settings.proxyPort).toBe(9090);
    expect(state.settingsVersion).toBe(v0 + 1);
  });

  it('saveSettings invokes update_settings', async () => {
    mockInvoke.mockResolvedValueOnce(undefined);
    await useVPNStore.getState().saveSettings();
    expect(mockInvoke).toHaveBeenCalledWith('update_settings', {
      newSettings: useVPNStore.getState().settings,
    });
  });

  it('saveSettings logs error on failure', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('save failed'));
    await useVPNStore.getState().saveSettings();
    expect(useVPNStore.getState().logs.some((l) => l.message.includes('Failed to save'))).toBe(
      true,
    );
  });

  it('loadSettings migrates old proxy mode', async () => {
    mockInvoke.mockResolvedValueOnce({ routingMode: 'proxy' });
    await useVPNStore.getState().loadSettings();
    expect(useVPNStore.getState().settings.routingMode).toBe('system-proxy');
  });

  it('loadSettings handles failure gracefully', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('fail'));
    await useVPNStore.getState().loadSettings();
    expect(
      useVPNStore.getState().logs.some((l) => l.message.includes('Failed to load settings')),
    ).toBe(true);
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 5. VPN connection toggling
// ─────────────────────────────────────────────────────────────────────────────
describe('store — toggleConnection', () => {
  it('disconnect path calls stop_vpn', async () => {
    useVPNStore.setState({ status: 'connected' });
    mockInvoke.mockResolvedValueOnce(undefined);
    await useVPNStore.getState().toggleConnection();
    expect(mockInvoke).toHaveBeenCalledWith('stop_vpn');
    expect(useVPNStore.getState().status).toBe('disconnecting');
  });

  it('connect path calls saveSettings then start_vpn', async () => {
    useVPNStore.setState({
      activeServer: { id: 'srv-1', name: 'Test', protocol: 'vless', server: '10.0.0.1', port: 443 },
      servers: [
        { id: 'srv-1', name: 'Test', protocol: 'vless', server: '10.0.0.1', port: 443 },
        { id: 'srv-2', name: 'Alt', protocol: 'vless', server: '10.0.0.2', port: 443 },
      ],
      realIp: '1.2.3.4',
    });
    mockInvoke.mockResolvedValueOnce(undefined); // saveSettings — update_settings
    mockInvoke.mockResolvedValueOnce(mockGetSubscriptions(useVPNStore.getState().servers)); // saveSettings — get_subscriptions
    mockInvoke.mockResolvedValueOnce(undefined); // start_vpn
    mockInvoke.mockResolvedValueOnce({ server: "5.6.7.8", flag: "PL", country_name: "Poland", city_name: "Warsaw" }); // outlet_ip

    await useVPNStore.getState().toggleConnection();

    expect(mockInvoke).toHaveBeenCalledWith(
      'start_vpn',
      expect.objectContaining({
        routingMode: 'vpn',
        proxyPort: 1080,
      }),
    );
    expect(useVPNStore.getState().status).toBe('connecting');
  });

  it('connect path populates alternativeServers when autoConnectFastest', async () => {
    const activeServer = {
      id: 'srv-1',
      name: 'Main',
      protocol: 'vless',
      server: '10.0.0.1',
      port: 443,
    } as never;
    useVPNStore.setState({
      activeServer,
      servers: [
        { id: 'srv-1', name: 'Main', server: '10.0.0.1', port: 443 },
        { id: 'srv-2', name: 'Alt1', server: '10.0.0.2', port: 443 },
        { id: 'srv-3', name: 'Alt2', server: '10.0.0.3', port: 443 },
      ],
      realIp: '1.2.3.4',
    });
    mockInvoke.mockResolvedValueOnce(undefined); // saveSettings — update_settings
    mockInvoke.mockResolvedValueOnce(mockGetSubscriptions(useVPNStore.getState().servers)); // saveSettings — get_subscriptions
    mockInvoke.mockResolvedValueOnce(undefined); // start_vpn
    mockInvoke.mockResolvedValueOnce({ server: "5.6.7.8", flag: "PL", country_name: "Poland", city_name: "Warsaw" }); // outlet_ip

    await useVPNStore.getState().toggleConnection();

    const startCall = mockInvoke.mock.calls.find((c) => c[0] === 'start_vpn');
    expect(startCall).toBeDefined();
    // alternativeServers should have 2 entries (all except active)
    expect(startCall![1].alternativeServers).toHaveLength(2);
  });

  it('connect path does NOT populate altServers when autoConnectFastest disabled', async () => {
    useVPNStore.setState({
      activeServer: { id: 'srv-1', name: 'Test', protocol: 'vless', server: '10.0.0.1', port: 443 },
      servers: [{ id: 'srv-1', name: 'Test', server: '10.0.0.1', port: 443 }],
      realIp: '1.2.3.4',
    });
    useVPNStore.getState().updateSettings({ autoConnectFastest: false });
    mockInvoke.mockResolvedValueOnce(undefined); // saveSettings — update_settings
    mockInvoke.mockResolvedValueOnce(mockGetSubscriptions(useVPNStore.getState().servers)); // saveSettings — get_subscriptions
    mockInvoke.mockResolvedValueOnce(undefined); // start_vpn
    mockInvoke.mockResolvedValueOnce({ server: "5.6.7.8", flag: "PL", country_name: "Poland", city_name: "Warsaw" }); // outlet_ip

    await useVPNStore.getState().toggleConnection();
    const startCall = mockInvoke.mock.calls.find((c) => c[0] === 'start_vpn');
    expect(startCall).toBeDefined();
    expect(startCall![1].alternativeServers).toHaveLength(0);
  });

  it('guards against reconnect during toggle', async () => {
    useVPNStore.setState({ _isReconnecting: true });
    await useVPNStore.getState().toggleConnection();
    // Should not call invoke at all
    expect(mockInvoke).not.toHaveBeenCalledWith('start_vpn');
    expect(mockInvoke).not.toHaveBeenCalledWith('stop_vpn');
    expect(
      useVPNStore.getState().logs.some((l) => l.message.includes('reconnect in progress')),
    ).toBe(true);
  });

  it('sets error state on connection failure', async () => {
    useVPNStore.setState({
      activeServer: { id: 'srv-1', name: 'Test', server: '10.0.0.1', port: 443 },
      realIp: '1.2.3.4',
    });
    mockInvoke.mockResolvedValueOnce(undefined); // saveSettings — update_settings
    mockInvoke.mockResolvedValueOnce(mockGetSubscriptions(useVPNStore.getState().servers)); // saveSettings — get_subscriptions
    mockInvoke.mockRejectedValueOnce(new Error('timeout')); // start_vpn fails

    await expect(useVPNStore.getState().toggleConnection()).rejects.toThrow('timeout');
    expect(useVPNStore.getState().status).toBe('error');
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 6. IP fetching
// ─────────────────────────────────────────────────────────────────────────────
describe('store — IP fetching', () => {
  it('fetchOutletIp stores IP on success', async () => {
    mockInvoke.mockResolvedValueOnce({ server: "1.2.3.4", flag: "PL", country_name: "Poland", city_name: "Warsaw" });
    await useVPNStore.getState().fetchExitIp();
    const result = useVPNStore.getState().exitIp;
    expect(result).not.toBeNull();
    expect(result?.server).toBe('1.2.3.4');
  });

  it('fetchOutletIp stores null on error', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('fail'));
    await useVPNStore.getState().fetchExitIp();
    expect(useVPNStore.getState().exitIp).toBeNull();
  });

  it('fetchRealIp stores IP on success', async () => {
    mockInvoke.mockResolvedValueOnce('9.9.9.9');
    await useVPNStore.getState().fetchRealIp();
    expect(useVPNStore.getState().realIp).toBe('9.9.9.9');
  });

  it('fetchRealIp stores null on error', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('fail'));
    await useVPNStore.getState().fetchRealIp();
    expect(useVPNStore.getState().realIp).toBeNull();
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
    const result = await useVPNStore.getState().runSpeedTest();
    expect(result?.delay).toBe(50);
    expect(useVPNStore.getState().speedTestResult?.delay).toBe(50);
  });

  it('runSpeedTest returns null on error', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('fail'));
    const result = await useVPNStore.getState().runSpeedTest();
    expect(result).toBeNull();
    expect(useVPNStore.getState().speedTestResult).toBeNull();
  });

  it('runSpeedTest returns null when no results', async () => {
    mockInvoke.mockResolvedValueOnce([]);
    const result = await useVPNStore.getState().runSpeedTest();
    expect(result).toBeNull();
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 8. Health check / reconnect
// ─────────────────────────────────────────────────────────────────────────────
describe('store — initListener health check', () => {
  beforeEach(function () {
    mockInvoke.mockResolvedValue(undefined);
    useVPNStore.setState({
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

  it('registers vpn-status-changed listener', async () => {
    const cleanup = await useVPNStore.getState().initListener();
    expect(mockListenFns.some((e) => e.event === 'vpn-status-changed')).toBe(true);
    cleanup();
  });

  it('registers log-entry and traffic-update listeners', async () => {
    const cleanup = await useVPNStore.getState().initListener();
    expect(mockListenFns.some((e) => e.event === 'log-entry')).toBe(true);
    expect(mockListenFns.some((e) => e.event === 'traffic-update')).toBe(true);
    cleanup();
  });

  it('vpn-status-changed connected starts health check interval', async () => {
    const cleanup = await useVPNStore.getState().initListener();
    expect(useVPNStore.getState()._healthCheckTimer).toBeNull();

    // Simulate connected event
    mockInvoke.mockResolvedValue(true); // check_proxy_health returns alive
    triggerEvent('vpn-status-changed', { status: 'connected' });

    expect(useVPNStore.getState().status).toBe('connected');
    expect(useVPNStore.getState().connectedAt).not.toBeNull();
    expect(useVPNStore.getState()._healthCheckTimer).not.toBeNull();
    cleanup();
  });

  it('vpn-status-changed disconnected stops health check', async () => {
    const cleanup = await useVPNStore.getState().initListener();
    triggerEvent('vpn-status-changed', { status: 'connected' });
    expect(useVPNStore.getState()._healthCheckTimer).not.toBeNull();

    triggerEvent('vpn-status-changed', { status: 'disconnected' });
    expect(useVPNStore.getState()._healthCheckTimer).toBeNull();
    expect(useVPNStore.getState().connectedAt).toBeNull();
    cleanup();
  });

  it('vpn-status-changed error stops health check', async () => {
    const cleanup = await useVPNStore.getState().initListener();
    triggerEvent('vpn-status-changed', { status: 'connected' });
    expect(useVPNStore.getState()._healthCheckTimer).not.toBeNull();

    triggerEvent('vpn-status-changed', { status: { error: 'lost' } });
    expect(useVPNStore.getState().status).toBe('error');
    expect(useVPNStore.getState()._healthCheckTimer).toBeNull();
    cleanup();
  });

  it('traffic-update updates speeds', async () => {
    const cleanup = await useVPNStore.getState().initListener();
    triggerEvent('traffic-update', { up: 12345, down: 67890 });
    expect(useVPNStore.getState().speeds).toEqual({ up: 12345, down: 67890 });
    cleanup();
  });

  it('log-entry event adds to log buffer', async () => {
    const cleanup = await useVPNStore.getState().initListener();
    expect(useVPNStore.getState().logs.length).toBe(0);
    triggerEvent('log-entry', {
      message: 'from rust',
      level: 'info',
      source: 'system',
      id: 'abc',
      timestamp: Date.now(),
    });
    expect(useVPNStore.getState().logs.length).toBe(1);
    expect(useVPNStore.getState().logs[0].message).toBe('from rust');
    cleanup();
  });

  it('cleanup function stops health check and unlistens', async () => {
    const cleanup = await useVPNStore.getState().initListener();
    triggerEvent('vpn-status-changed', { status: 'connected' });
    const timer = useVPNStore.getState()._healthCheckTimer;
    expect(timer).not.toBeNull();

    cleanup();
    expect(useVPNStore.getState()._healthCheckTimer).toBeNull();
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 9. pickReplacementServer
// ─────────────────────────────────────────────────────────────────────────────
describe('pickReplacementServer', () => {
  it('returns null when no servers available', () => {
    const result = pickReplacementServer(
      [],
      { id: 'srv-1', name: 'Dead' },
      useVPNStore.getState().settings,
    );
    expect(result).toBeNull();
  });

  it('prefers same country with fastest ping', () => {
    const servers = [
      { id: 'srv-ch-1', name: 'CH Fast', flag: 'CH', ping: 50 } as never,
      { id: 'srv-ch-2', name: 'CH Slow', flag: 'CH', ping: 200 } as never,
      { id: 'srv-de', name: 'DE', flag: 'DE', ping: 100 } as never,
    ];
    const current = { id: 'srv-ch-1', name: 'Old', flag: 'CH' } as never;
    const result = pickReplacementServer(servers, current, useVPNStore.getState().settings);
    // Same country (CH) -> picks fastest CH server that's not current
    expect(result?.id).toBe('srv-ch-2');
  });

  it('falls back to fastest any country when no same country available', () => {
    const servers = [
      { id: 'srv-us', name: 'US', flag: 'US', ping: 300 } as never,
      { id: 'srv-de', name: 'DE Fast', flag: 'DE', ping: 50 } as never,
    ];
    const current = { id: 'srv-ch', name: 'Old', flag: 'CH' } as never;
    const result = pickReplacementServer(servers, current, useVPNStore.getState().settings);
    expect(result?.id).toBe('srv-de');
  });

  it('excludes the dead server itself', () => {
    const servers = [
      { id: 'srv-dead', name: 'Dead', flag: 'CH', ping: 50 } as never,
      { id: 'srv-other', name: 'Other', flag: 'DE', ping: 100 } as never,
    ];
    const current = { id: 'srv-dead', name: 'Dead', flag: 'CH' } as never;
    const result = pickReplacementServer(servers, current, useVPNStore.getState().settings);
    expect(result?.id).toBe('srv-other');
  });

  it('applies allowedCountries filter', () => {
    const state = useVPNStore.getState();
    state.updateSettings({ allowedCountries: ['DE'] });
    const servers = [
      { id: 'srv-us', name: 'US', flag: 'US', ping: 10 } as never,
      { id: 'srv-de', name: 'DE', flag: 'DE', ping: 100 } as never,
    ];
    const current = { id: 'srv-dead', name: 'Dead', flag: 'CH' } as never;
    const result = pickReplacementServer(servers, current, useVPNStore.getState().settings);
    expect(result?.id).toBe('srv-de');
  });

  it('applies blockedCountries filter', () => {
    const state = useVPNStore.getState();
    state.updateSettings({ blockedCountries: ['US'] });
    const servers = [
      { id: 'srv-us', name: 'US', flag: 'US', ping: 10 } as never,
      { id: 'srv-de', name: 'DE', flag: 'DE', ping: 100 } as never,
    ];
    const current = { id: 'srv-dead', name: 'Dead', flag: 'CH' } as never;
    const result = pickReplacementServer(servers, current, useVPNStore.getState().settings);
    expect(result?.id).toBe('srv-de');
  });

  it('applies blockCdnNodes filter', () => {
    const state = useVPNStore.getState();
    state.updateSettings({ blockCdnNodes: true });
    const servers = [
      { id: 'srv-cdn', name: 'CDN', flag: 'US', ping: 10, behindCdn: true } as never,
      { id: 'srv-real', name: 'Real', flag: 'US', ping: 50, behindCdn: false } as never,
    ];
    const current = { id: 'srv-dead', name: 'Dead', flag: 'CH' } as never;
    const result = pickReplacementServer(servers, current, useVPNStore.getState().settings);
    expect(result?.id).toBe('srv-real');
  });

  it('applies allowedProtocols filter', () => {
    const state = useVPNStore.getState();
    state.updateSettings({ allowedProtocols: ['vless'] });
    const servers = [
      { id: 'srv-vless', name: 'VLESS', protocol: 'vless', flag: 'US', ping: 50 } as never,
      { id: 'srv-trojan', name: 'Trojan', protocol: 'trojan', flag: 'US', ping: 30 } as never,
    ];
    const current = { id: 'srv-dead', name: 'Dead', flag: 'CH' } as never;
    const result = pickReplacementServer(servers, current, useVPNStore.getState().settings);
    expect(result?.id).toBe('srv-vless');
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 10. refreshConnections
// ─────────────────────────────────────────────────────────────────────────────
describe('store — connections', () => {
  it('refreshConnections stores connection list', async () => {
    mockInvoke.mockResolvedValueOnce([{ id: 'conn-1', host: 'example.com' }]);
    await useVPNStore.getState().refreshConnections();
    expect(useVPNStore.getState().connections).toHaveLength(1);
    expect(useVPNStore.getState().connections[0].host).toBe('example.com');
  });

  it('refreshConnections handles error gracefully', async () => {
    mockInvoke.mockRejectedValueOnce(new Error('not connected'));
    await useVPNStore.getState().refreshConnections();
    expect(useVPNStore.getState().connections).toEqual([]);
  });
});
