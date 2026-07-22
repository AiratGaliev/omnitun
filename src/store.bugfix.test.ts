/* eslint-disable @typescript-eslint/require-await,@typescript-eslint/no-empty-function,@typescript-eslint/no-floating-promises,@typescript-eslint/no-unnecessary-condition,@typescript-eslint/no-unsafe-assignment */
/**
 * Regression tests for the profile add/refresh UX-flow:
 *   - addProfile: pending profile appears immediately, _refreshingIds tracked,
 *     error cleanup, profileId passed to backend
 *   - importProfileContent: same pending-profile pattern
 *   - servers-discovered (deprecated → server-tested): updates both state.servers and profile.servers
 *     incrementally; orphaned servers guarded
 *   - refreshProfile: lastUpdated set to now on error (not future)
 *
 * NOTE: these tests use vi.resetModules() + re-import to get a fresh store
 * for each describe block. They are isolated from store.test.ts.
 */

import { describe, expect, it, vi } from 'vitest';

// ── Tauri IPC mocks ──────────────────────────────────────────────────────

type InvokeHandler = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
type ListenHandler = (
  event: string,
  handler: (event: { payload: unknown }) => void,
) => Promise<() => void>;

let invokeHandler: InvokeHandler = () => Promise.resolve(undefined);
let listenHandlers: Record<string, ((event: { payload: unknown }) => void)[]> = {};

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args?: Record<string, unknown>) => invokeHandler(cmd, args),
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: ((event: string, handler: (event: { payload: unknown }) => void) => {
    if (!listenHandlers[event]) listenHandlers[event] = [];
    listenHandlers[event].push(handler);
    return Promise.resolve(() => {
      const idx = listenHandlers[event].indexOf(handler);
      if (idx >= 0) listenHandlers[event].splice(idx, 1);
    });
  }) as ListenHandler,
}));

// ── Helpers ──────────────────────────────────────────────────────────────

function makeRustServer(id: string, overrides: Record<string, unknown> = {}) {
  return {
    id,
    protocol: 'vless',
    server: `10.0.0.${id.slice(-1)}`,
    port: 443,
    ping_ms: 50,
    country_name: 'United States',
    city_name: 'New York',
    flag: 'US',
    ...overrides,
  };
}

function makeRustProfile(id: string, servers: ReturnType<typeof makeRustServer>[] = []) {
  return {
    id,
    name: `Profile ${id}`,
    url: `https://example.com/${id}`,
    servers,
    updated_at: new Date().toISOString(),
    last_tested: null,
    traffic_used: null,
    traffic_total: null,
    traffic_expiry: null,
  };
}

async function initStore() {
  // Reset modules so the store is re-created fresh for each test
  vi.resetModules();
  listenHandlers = {};
  invokeHandler = async () => undefined;

  // Re-import the store after resetting modules
  const { useAppStore } = await import('./store');
  // Initialize the listener (sets up listen() subscriptions)
  await useAppStore.getState().initListener();
  return { useAppStore };
}

// Emit a Tauri event to all registered listeners
function emit(event: string, payload: unknown) {
  for (const handler of listenHandlers[event] ?? []) {
    handler({ payload });
  }
}

// ── Tests ────────────────────────────────────────────────────────────────

describe('addProfile: pending profile appears immediately', () => {
  it('profile appears in state.profiles before backend completes', async () => {
    const { useAppStore } = await initStore();
    invokeHandler = vi.fn(() => new Promise(() => {})); // never resolves

    useAppStore.getState().addProfile({ name: 'Pending', url: 'https://example.com/p' });

    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.profiles[0].name).toBe('Pending');
    expect(state.profiles[0].servers).toHaveLength(0);
  });

  it('profile ID is added to _refreshingIds during the call', async () => {
    const { useAppStore } = await initStore();
    invokeHandler = vi.fn(() => new Promise(() => {})); // never resolves

    useAppStore.getState().addProfile({ name: 'Refreshing', url: 'https://example.com/r' });

    const state = useAppStore.getState();
    expect(state._refreshingIds).toHaveLength(1);
    expect(state._refreshingIds[0]).toBe(state.profiles[0].id);
  });
});

describe('addProfile: error cleanup removes pending profile', () => {
  it('pending profile is removed when backend fails', async () => {
    const { useAppStore } = await initStore();
    invokeHandler = vi.fn().mockRejectedValueOnce(new Error('Network error'));

    await expect(
      useAppStore.getState().addProfile({ name: 'Fail', url: 'https://example.com/fail' }),
    ).rejects.toThrow('Network error');

    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(0);
    expect(state._refreshingIds).toHaveLength(0);
  });

  it('_refreshingIds is cleared in finally block even on error', async () => {
    const { useAppStore } = await initStore();
    invokeHandler = vi.fn().mockRejectedValueOnce(new Error('fail'));

    await expect(
      useAppStore.getState().addProfile({ name: 'Fail', url: 'https://example.com/f' }),
    ).rejects.toThrow();

    expect(useAppStore.getState()._refreshingIds).toHaveLength(0);
  });
});

describe('servers-discovered: updates profile.servers incrementally', () => {
  it.skip('profile.servers grows as server-tested events arrive', async () => {
    const { useAppStore } = await initStore();

    // Set up: add a pending profile (backend hangs)
    invokeHandler = vi.fn(() => new Promise(() => {}));
    useAppStore.getState().addProfile({ name: 'Stream', url: 'https://example.com/s' });

    const profileId = useAppStore.getState().profiles[0].id;
    expect(useAppStore.getState().profiles[0].servers).toHaveLength(0);

    // Emit a servers-discovered chunk for this profile
    emit('servers-discovered', {
      profile_id: profileId,
      servers: [makeRustServer('srv-1'), makeRustServer('srv-2')],
    });

    // Wait for the 150ms flush
    await new Promise((r) => setTimeout(r, 200));

    const state = useAppStore.getState();
    expect(state.servers).toHaveLength(2);
    expect(state.profiles[0].servers).toHaveLength(2);
    expect(state.profiles[0].servers[0].id).toBe('srv-1');
  });

  it.skip('multiple server-tested events accumulate in profile.servers', async () => {
    const { useAppStore } = await initStore();
    invokeHandler = vi.fn(() => new Promise(() => {}));
    useAppStore.getState().addProfile({ name: 'Multi', url: 'https://example.com/m' });

    const profileId = useAppStore.getState().profiles[0].id;

    emit('servers-discovered', {
      profile_id: profileId,
      servers: [makeRustServer('a'), makeRustServer('b')],
    });
    await new Promise((r) => setTimeout(r, 200));

    emit('servers-discovered', {
      profile_id: profileId,
      servers: [makeRustServer('c')],
    });
    await new Promise((r) => setTimeout(r, 200));

    const state = useAppStore.getState();
    expect(state.profiles[0].servers).toHaveLength(3);
    expect(state.servers).toHaveLength(3);
  });
});

describe('refreshProfile: error does not set future lastUpdated', () => {
  it('lastUpdated is set to ~now (not future) on refresh error', async () => {
    const { useAppStore } = await initStore();

    // Set up: load a profile first.
    invokeHandler = vi.fn().mockImplementation(async (cmd: string) => {
      if (cmd === 'get_subscriptions') return [makeRustProfile('p1', [makeRustServer('s1')])];
      if (cmd === 'pick_replacement_server') return null;
      return undefined;
    });
    await useAppStore.getState().loadProfiles();

    const before = Date.now();
    const profile = useAppStore.getState().profiles[0];

    // Make refresh fail
    invokeHandler = vi.fn().mockImplementation(async (cmd: string) => {
      if (cmd === 'refresh_subscription') throw new Error('refresh failed');
      return undefined;
    });
    await expect(useAppStore.getState().refreshProfile(profile.id)).rejects.toThrow();
    const after = Date.now();

    const updatedProfile = useAppStore.getState().profiles.find((p) => p.id === profile.id);
    expect(updatedProfile).toBeDefined();
    expect(updatedProfile!.lastUpdated).toBeGreaterThanOrEqual(before);
    expect(updatedProfile!.lastUpdated).toBeLessThanOrEqual(after);
    // CRITICAL: lastUpdated must NOT be in the future
    expect(updatedProfile!.lastUpdated).toBeLessThanOrEqual(Date.now());
  });

  it('timeAgo is never negative after a refresh error', async () => {
    const { useAppStore } = await initStore();

    invokeHandler = vi.fn().mockImplementation(async (cmd: string) => {
      if (cmd === 'get_subscriptions') return [makeRustProfile('p1', [makeRustServer('s1')])];
      if (cmd === 'pick_replacement_server') return null;
      return undefined;
    });
    await useAppStore.getState().loadProfiles();

    invokeHandler = vi.fn().mockImplementation(async (cmd: string) => {
      if (cmd === 'refresh_subscription') throw new Error('fail');
      return undefined;
    });
    await expect(useAppStore.getState().refreshProfile('p1')).rejects.toThrow();

    const profile = useAppStore.getState().profiles[0];
    const timeAgo = profile.lastUpdated
      ? Math.floor((Date.now() - profile.lastUpdated) / 60000)
      : 0;
    expect(timeAgo).toBeGreaterThanOrEqual(0);
  });
});

describe('addProfile: passes profileId to backend', () => {
  it('invoke receives the pre-generated profileId', async () => {
    const { useAppStore } = await initStore();

    const mockInvoke = vi
      .fn()
      .mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
        if (cmd === 'add_subscription') {
          return makeRustProfile(args?.profileId as string, [makeRustServer('s1')]);
        }
        if (cmd === 'get_subscriptions') return [];
        if (cmd === 'get_settings') return null;
        if (cmd === 'get_status') return 'disconnected';
        if (cmd === 'get_logs') return [];
        if (cmd === 'get_real_ip') return null;
        if (cmd === 'pick_replacement_server') return null;
        return undefined;
      });
    invokeHandler = mockInvoke;

    await useAppStore.getState().addProfile({ name: 'WithId', url: 'https://example.com/id' });

    expect(mockInvoke).toHaveBeenCalledWith(
      'add_subscription',
      expect.objectContaining({
        name: 'WithId',
        url: 'https://example.com/id',
        profileId: expect.any(String),
      }),
    );

    // The profile in the store should have the same ID as what was passed
    const storedProfile = useAppStore.getState().profiles[0];
    const callArgs = mockInvoke.mock.calls.find((c) => c[0] === 'add_subscription')?.[1] as Record<
      string,
      unknown
    >;
    expect(storedProfile.id).toBe(callArgs.profileId);
  });
});

describe('addProfile success path', () => {
  it('pending profile is replaced with real one (same ID) on success', async () => {
    const { useAppStore } = await initStore();

    invokeHandler = vi
      .fn()
      .mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
        if (cmd === 'add_subscription') {
          return makeRustProfile(args?.profileId as string, [
            makeRustServer('s1'),
            makeRustServer('s2'),
          ]);
        }
        if (cmd === 'pick_replacement_server') return null;
        return undefined;
      });

    await useAppStore.getState().addProfile({ name: 'Success', url: 'https://example.com/ok' });

    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.profiles[0].servers).toHaveLength(2);
    expect(state._refreshingIds).toHaveLength(0);
  });
});

describe('orphaned servers guard in servers-discovered flush', () => {
  // Deprecated: servers-discovered was replaced by server-tested.
  // These tests are preserved for reference but skipped because
  // the event no longer exists in the backend.
  it.skip('servers-discovered for a non-existent profile does NOT add servers to state.servers', async () => {
    const { useAppStore } = await initStore();

    // No profiles in the store. Emit a servers-discovered event for a
    // non-existent profile_id.
    emit('servers-discovered', {
      profile_id: 'non-existent-profile',
      servers: [makeRustServer('orphan-1'), makeRustServer('orphan-2')],
    });

    // Wait for the 150ms flush
    await new Promise((r) => setTimeout(r, 200));

    const state = useAppStore.getState();
    // CRITICAL: no servers should be added because the profile doesn't exist
    expect(state.servers).toHaveLength(0);
    expect(state.profiles).toHaveLength(0);
  });

  it.skip('after addProfile fails, late servers-discovered events do not leave orphaned servers', async () => {
    const { useAppStore } = await initStore();

    // Start addProfile — it will fail
    invokeHandler = vi.fn().mockImplementation(async (cmd: string) => {
      if (cmd === 'add_subscription') throw new Error('backend failed');
      return undefined;
    });

    await expect(
      useAppStore.getState().addProfile({ name: 'Will Fail', url: 'https://example.com/fail' }),
    ).rejects.toThrow('backend failed');

    // Emit a late servers-discovered event for any non-existent profile ID
    emit('servers-discovered', {
      profile_id: 'any-removed-profile-id',
      servers: [makeRustServer('orphan-after-fail')],
    });

    await new Promise((r) => setTimeout(r, 200));

    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(0);
    expect(state.servers).toHaveLength(0);
  });
});

describe('importProfileContent: pending-profile pattern', () => {
  it('profile appears in store immediately (before backend completes)', async () => {
    const { useAppStore } = await initStore();
    invokeHandler = vi.fn(() => new Promise(() => {})); // never resolves

    useAppStore.getState().importProfileContent({ name: 'Import', content: 'vless://...' });

    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(1);
    expect(state.profiles[0].name).toBe('Import');
    expect(state.profiles[0].servers).toHaveLength(0);
    expect(state._refreshingIds).toHaveLength(1);
  });

  it('passes profileId to backend import_subscription_content', async () => {
    const { useAppStore } = await initStore();

    const mockInvoke = vi
      .fn()
      .mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
        if (cmd === 'import_subscription_content') {
          return makeRustProfile(args?.profileId as string, [makeRustServer('s1')]);
        }
        if (cmd === 'pick_replacement_server') return null;
        return undefined;
      });
    invokeHandler = mockInvoke;

    await useAppStore.getState().importProfileContent({ name: 'WithId', content: 'vless://...' });

    expect(mockInvoke).toHaveBeenCalledWith(
      'import_subscription_content',
      expect.objectContaining({
        name: 'WithId',
        content: 'vless://...',
        profileId: expect.any(String),
      }),
    );
  });

  it.skip('server-tested during import updates profile.servers incrementally', async () => {
    const { useAppStore } = await initStore();
    invokeHandler = vi.fn(() => new Promise(() => {})); // never resolves

    useAppStore.getState().importProfileContent({ name: 'Stream', content: 'vless://...' });
    const profileId = useAppStore.getState().profiles[0].id;

    emit('servers-discovered', {
      profile_id: profileId,
      servers: [makeRustServer('imp-1')],
    });
    await new Promise((r) => setTimeout(r, 200));

    const state = useAppStore.getState();
    expect(state.profiles[0].servers).toHaveLength(1);
    expect(state.profiles[0].servers[0].id).toBe('imp-1');
    expect(state.servers).toHaveLength(1);
  });

  it('on error, pending profile and _refreshingIds are cleaned up atomically', async () => {
    const { useAppStore } = await initStore();
    invokeHandler = vi.fn().mockImplementation(async (cmd: string) => {
      if (cmd === 'import_subscription_content') throw new Error('import failed');
      return undefined;
    });

    await expect(
      useAppStore.getState().importProfileContent({ name: 'Fail', content: 'vless://...' }),
    ).rejects.toThrow('import failed');

    const state = useAppStore.getState();
    expect(state.profiles).toHaveLength(0);
    expect(state._refreshingIds).toHaveLength(0);
    expect(state.servers).toHaveLength(0);
  });
});
