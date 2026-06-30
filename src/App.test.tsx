import { describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import App from './App';

// Mock Tauri invoke to return sensible defaults for tests
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn().mockImplementation((cmd: string) => {
    if (cmd === 'get_settings') {
      return Promise.resolve({
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
      });
    }
    if (cmd === 'get_subscriptions') {
      return Promise.resolve([]);
    }
    return Promise.resolve(undefined);
  }),
}));

describe('App', () => {
  it('renders the Terms screen on first launch', async () => {
    render(<App />);
    await waitFor(() => {
      expect(screen.getByText('Terms of Service')).toBeInTheDocument();
    });
    expect(screen.getByText('Accept and Continue')).toBeInTheDocument();
  });
});
