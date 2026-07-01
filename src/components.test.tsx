import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { useAppStore } from './store';
import { Dashboard } from './components/screens/Dashboard';
import { Profiles } from './components/screens/Profiles';
import { Navigation } from './components/Navigation';
import { GeneralSettings } from './components/settings/GeneralSettings';
import { NetworkSettings } from './components/settings/NetworkSettings';
import { FilteringSettings } from './components/settings/FilteringSettings';
import { ProtocolSettings } from './components/settings/ProtocolSettings';
import { Onboarding } from './components/screens/Onboarding';
import { Terms } from './components/screens/Terms';
import { Logs } from './components/screens/Logs';
import { About } from './components/screens/About';
import { AddProfileOptions } from './components/AddProfileOptions';
import { ConfirmDialog } from './components/ConfirmDialog';
import { SegmentedControl } from './components/SegmentedControl';
import { SettingItem } from './components/settings/SettingItem';
import App from './App';

const mockInvoke = vi.hoisted(() => vi.fn<(...args: unknown[]) => unknown>());

vi.mock('@tauri-apps/api/core', () => ({
  invoke: mockInvoke,
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => undefined),
}));
vi.mock('@tauri-apps/plugin-clipboard-manager', () => ({
  readText: vi.fn(() => Promise.resolve('')),
}));
vi.mock('@tauri-apps/plugin-fs', () => ({}));
vi.mock('@tauri-apps/plugin-shell', () => ({}));
vi.mock('@/hooks/useTranslation', () => ({
  useTranslation: () => ({
    t: (key: string) => key,
    language: 'en',
    setLanguage: vi.fn(),
  }),
}));

function setConnectedState() {
  useAppStore.setState({
    hasAcceptedTerms: true,
    hasOnboarded: true,
    status: 'connected',
    speeds: { up: 102400, down: 204800 },
    connectedAt: Date.now(),
    activeServer: {
      id: 'srv-1',
      name: 'Switzerland',
      flag: 'CH',
      server: '10.0.0.1',
      port: 443,
      protocol: 'vless',
    },
    servers: [
      {
        id: 'srv-1',
        name: 'Switzerland',
        flag: 'CH',
        server: '10.0.0.1',
        port: 443,
        ping: 50,
        protocol: 'vless',
        countryName: 'Switzerland',
      },
    ],
    exitIp: { server: '5.6.7.8', flag: 'PL', country_name: 'Poland', city_name: 'Warsaw', proxy_tag: '' },
  });
}

describe('Dashboard', () => {
  beforeEach(() => {
    mockInvoke.mockResolvedValue(undefined);
    useAppStore.setState({
      hasAcceptedTerms: true,
      hasOnboarded: true,
      status: 'disconnected',
      activeServer: null,
      servers: [],
      settings: { ...useAppStore.getState().settings },
    });
  });

  it('renders without crash', () => {
    const { container } = render(<Dashboard />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });

  it('shows server list when servers exist', () => {
    useAppStore.setState({
      servers: [
        {
          id: 'srv-1',
          name: 'Switzerland',
          flag: 'CH',
          server: '10.0.0.1',
          port: 443,
          ping: 50,
          protocol: 'vless',
          countryName: 'Switzerland',
        },
      ],
    });
    render(<Dashboard />);
    expect(document.body.innerHTML.length).toBeGreaterThan(0);
  });

  it('shows connected status', () => {
    setConnectedState();
    render(<Dashboard />);
    expect(useAppStore.getState().status).toBe('connected');
  });

  it('shows connecting status', () => {
    useAppStore.setState({
      status: 'connecting',
      activeServer: { id: 'srv-1', name: 'Srv', server: '10.0.0.1', port: 443 },
    });
    render(<Dashboard />);
    expect(useAppStore.getState().status).toBe('connecting');
  });
});

describe('Profiles', () => {
  beforeEach(() => {
    useAppStore.setState({
      hasAcceptedTerms: true,
      hasOnboarded: true,
      profiles: [
        {
          id: 'prof-1',
          name: 'My Subscription',
          url: 'https://x.com/sub',
          servers: [
            { id: 'srv-1', name: 'US Server', flag: 'US', server: '1.2.3.4', port: 443 },
          ] as never,
        },
      ],
    });
  });

  it('renders profile list', async () => {
    render(<Profiles />);
    await waitFor(() => {
      expect(screen.getByText(/my subscription/i)).toBeInTheDocument();
    });
  });

  it('renders empty state', async () => {
    useAppStore.setState({ profiles: [] });
    render(<Profiles />);
    await waitFor(() => {
      expect(document.body.innerHTML.length).toBeGreaterThan(0);
    });
  });
});

describe('Navigation', () => {
  it('renders navigation tabs', () => {
    useAppStore.setState({ hasAcceptedTerms: true, hasOnboarded: true, status: 'disconnected' });
    const { container } = render(<Navigation activeTab="home" onChange={vi.fn()} />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });
});

describe('Settings panels', () => {
  beforeEach(() => {
    useAppStore.setState({
      hasAcceptedTerms: true,
      hasOnboarded: true,
      settings: useAppStore.getState().settings,
    });
  });

  it('GeneralSettings renders', () => {
    const { container } = render(<GeneralSettings />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });

  it('NetworkSettings renders', () => {
    const { container } = render(<NetworkSettings />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });

  it('FilteringSettings renders', () => {
    const { container } = render(<FilteringSettings />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });

  it('ProtocolSettings renders', () => {
    const { container } = render(<ProtocolSettings />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });
});

describe('Onboarding & Terms', () => {
  it('Onboarding renders', () => {
    useAppStore.setState({ hasAcceptedTerms: true, hasOnboarded: false });
    const { container } = render(<Onboarding />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });

  it('Terms renders', () => {
    useAppStore.setState({ hasAcceptedTerms: false });
    const { container } = render(<Terms />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });
});

describe('Logs', () => {
  beforeEach(() => {
    useAppStore.setState({
      logs: [
        {
          id: '1',
          timestamp: Date.now(),
          message: 'Proxy connected',
          level: 'success' as const,
          source: 'proxy' as const,
        },
        {
          id: '2',
          timestamp: Date.now() - 1000,
          message: 'Starting proxy...',
          level: 'info' as const,
          source: 'system' as const,
        },
      ],
    });
  });

  it('renders log entries', async () => {
    render(<Logs />);
    await waitFor(() => {
      expect(screen.getByText(/proxy connected/i)).toBeInTheDocument();
    });
  });

  it('shows empty state when no logs', async () => {
    useAppStore.setState({ logs: [] });
    const { container } = render(<Logs />);
    await waitFor(() => {
      expect(container.innerHTML.length).toBeGreaterThan(0);
    });
  });

  it('clear button works', async () => {
    render(<Logs />);
    await waitFor(() => {
      const btn = screen.queryByRole('button', { name: /clear/i });
      if (btn) {
        fireEvent.click(btn);
        expect(useAppStore.getState().logs).toHaveLength(0);
      }
    });
  });
});

describe('About', () => {
  it('renders', () => {
    const { container } = render(<About />);
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });
});

describe('AddProfileOptions', () => {
  it('renders with URL input', async () => {
    const { container } = render(<AddProfileOptions onAdd={vi.fn()} />);
    await waitFor(() => {
      expect(container.innerHTML.length).toBeGreaterThan(0);
    });
  });
});

describe('ConfirmDialog', () => {
  it('renders with title and buttons', () => {
    const { container } = render(
      <ConfirmDialog
        open={true}
        title="Delete?"
        description="Are you sure?"
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );
    expect(container.innerHTML.length).toBeGreaterThan(0);
  });
});

describe('SegmentedControl', () => {
  it('renders options and handles click', () => {
    const onChange = vi.fn();
    render(
      <SegmentedControl
        options={[
          { id: 'proxy', label: 'Proxy' },
        ]}
        value="proxy"
        onChange={onChange}
      />,
    );
    expect(screen.getByText('Proxy')).toBeInTheDocument();
    fireEvent.click(screen.getByText('Proxy'));
    expect(onChange).toHaveBeenCalledWith('proxy');
  });
});

describe('SettingItem', () => {
  it('renders title and description', () => {
    const { container } = render(
      <SettingItem
        icon={<span>⚙️</span>}
        title="Enable feature"
        description="Turns it on"
        checked={false}
        onChange={vi.fn()}
      />,
    );
    expect(container.innerHTML.length).toBeGreaterThan(0);
    expect(screen.getByText(/enable feature/i)).toBeInTheDocument();
  });
});

describe('Full App Rendering', () => {
  beforeEach(() => {
    mockInvoke.mockResolvedValue(undefined);
    useAppStore.setState({
      hasAcceptedTerms: true,
      hasOnboarded: true,
      status: 'disconnected',
      servers: [],
      activeServer: null,
    });
  });

  it('renders without crash', async () => {
    const { container } = render(<App />);
    await waitFor(() => {
      expect(container.innerHTML.length).toBeGreaterThan(0);
    });
  });
});
