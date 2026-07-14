import { useEffect, useRef, useState } from 'react';
import { AnimatePresence, motion } from 'framer-motion';
import { Check, ChevronDown, Gauge, Globe, Loader2, Plus, Power, Server } from 'lucide-react';
import { toast } from 'sonner';
import { getExitIpDisplayName, getServerDisplayName, useAppStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { i18n } from '@/i18n.ts';
import { SegmentedControl } from '../SegmentedControl';
import { hasFlag } from 'country-flag-icons';
import 'country-flag-icons/3x2/flags.css';
import type { Tab } from '@/types.ts';

/** Render flag via country-flag-icons library. Falls back to globe emoji. */
function FlagDisplay({ code, className }: Readonly<{ code?: string; className?: string }>) {
  const cc = code?.toUpperCase();
  if (cc?.length === 2 && hasFlag(cc)) {
    return <span className={`${className ?? ''} flag:${cc}`} />;
  }
  return <span className={className ?? ''}>🌐</span>;
}

function formatProtocol(protocol: string): string {
  const map: Record<string, string> = {
    vless: 'VLESS',
    vmess: 'VMess',
    shadowsocks: 'Shadowsocks',
    trojan: 'Trojan',
    hysteria2: 'Hysteria 2',
    tuic: 'TUIC',
    wireguard: 'WireGuard',
  };
  return map[protocol] ?? protocol.charAt(0).toUpperCase() + protocol.slice(1);
}

// ── Cap rendered server dropdown (UI freeze fix for huge subscriptions) ───
// Rendering 7000 buttons (one per server in a huge subscription) causes a
// multi-second main-thread freeze. We cap to 200. Servers arrive pre-sorted
// by ping ascending, so the top 200 are the fastest.
const MAX_DROPDOWN_SERVERS = 200;

export function Dashboard({ onNavigate }: { readonly onNavigate?: (tab: Tab) => void }) {
  // ── Selective selectors (UI freeze fix) ─────────────────────────────────
  // Previously Dashboard used `useAppStore()` with no selector, subscribing
  // to the ENTIRE store. Every store change — log entries (hundreds during
  // a 7000-server batch test), refresh-progress flushes (8/s), connection
  // list updates, speed tests — triggered a full Dashboard re-render.
  //
  // Selective selectors ensure Dashboard only re-renders when the specific
  // slices it uses actually change.
  const status = useAppStore((s) => s.status);
  const activeServer = useAppStore((s) => s.activeServer);
  const servers = useAppStore((s) => s.servers);
  const speeds = useAppStore((s) => s.speeds);
  const connectedAt = useAppStore((s) => s.connectedAt);
  const setSpeeds = useAppStore((s) => s.setSpeeds);
  const setActiveServer = useAppStore((s) => s.setActiveServer);
  const settings = useAppStore((s) => s.settings);
  const updateSettings = useAppStore((s) => s.updateSettings);
  const exitIp = useAppStore((s) => s.exitIp);
  const fetchExitIp = useAppStore((s) => s.fetchExitIp);
  const realIp = useAppStore((s) => s.realIp);
  const speedTestResult = useAppStore((s) => s.speedTestResult);
  const runSpeedTest = useAppStore((s) => s.runSpeedTest);
  const toggleConnection = useAppStore((state) => state.toggleConnection);
  const addLog = useAppStore((state) => state.addLog);
  // Track whether ANY profile is currently being refreshed (add or refresh).
  // When true and no servers are available yet, the server picker shows
  // "Обновление профиля" and the connect button is disabled — matching the
  // user's desired UX: "если серверов нет, но происходит обновление профиля,
  // блокировать список доступных узлов и кнопку подключения/отключения".
  const refreshingIds = useAppStore((state) => state._refreshingIds);
  const isAnyProfileRefreshing = refreshingIds.length > 0;
  const { t } = useTranslation();

  const formatElapsed = (since: number): string => {
    const diff = Math.floor((Date.now() - since) / 1000);
    const h = Math.floor(diff / 3600)
      .toString()
      .padStart(2, '0');
    const m = Math.floor((diff % 3600) / 60)
      .toString()
      .padStart(2, '0');
    const s = (diff % 60).toString().padStart(2, '0');
    return `${h}:${m}:${s}`;
  };

  const formatSpeed = (mbs: number): { value: string; unit: string } => {
    const bps = mbs * 1_048_576; // Rust backend sends MB/s, convert to B/s
    if (bps >= 1_073_741_824) {
      return { value: (bps / 1_073_741_824).toFixed(1), unit: t('speedGbps') };
    }
    if (bps >= 1_048_576) {
      return { value: (bps / 1_048_576).toFixed(1), unit: t('speedMbps') };
    }
    if (bps >= 1024) {
      return { value: (bps / 1024).toFixed(1), unit: t('speedKbps') };
    }
    return { value: bps.toFixed(1), unit: t('speedBps') };
  };

  const [isServerDropdownOpen, setIsServerDropdownOpen] = useState(false);
  const [isSpeedTesting, setIsSpeedTesting] = useState(false);
  const [sessionDuration, setSessionDuration] = useState<string>(() =>
    connectedAt ? formatElapsed(connectedAt) : '00:00:00',
  );

  // Speed sparkline — ring buffer of last 30 values
  const [sparklineData, setSparklineData] = useState<number[]>([]);
  const sparklineRef = useRef(sparklineData);

  useEffect(() => {
    sparklineRef.current = sparklineData;
  }, [sparklineData]);

  useEffect(() => {
    if (status !== 'connected' || !connectedAt) return;
    const interval = setInterval(() => {
      setSessionDuration(formatElapsed(connectedAt));
    }, 1000);
    return () => {
      clearInterval(interval);
    };
  }, [status, connectedAt]);

  useEffect(() => {
    if (status !== 'connected') {
      setSpeeds({ up: 0, down: 0 });
    }
  }, [status, setSpeeds]);

  useEffect(() => {
    if (status === 'connected') {
      void fetchExitIp();
    }
  }, [status, fetchExitIp]);

  useEffect(() => {
    if (status !== 'connected') return;
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setSparklineData((prev) => {
      const s = useAppStore.getState().speeds;
      const total = s.down + s.up;
      const next = [...prev, total];
      return next.length > 30 ? next.slice(next.length - 30) : next;
    });
    const interval = setInterval(() => {
      setSparklineData((prev) => {
        const s = useAppStore.getState().speeds;
        const total = s.down + s.up;
        const next = [...prev, total];
        return next.length > 30 ? next.slice(next.length - 30) : next;
      });
    }, 1000);
    return () => {
      clearInterval(interval);
    };
  }, [status]);

  const handleSpeedTest = async () => {
    setIsSpeedTesting(true);
    const result = await runSpeedTest();
    if (result?.delay) {
      toast.success(`Latency: ${String(result.delay)} ms`);
    } else {
      addLog('Speed test returned no result', 'warning', 'system');
      toast.error('Speed test failed');
    }
    setIsSpeedTesting(false);
  };

  /** Бесшовный реконнект: отключаем → ждём 500ms → включаем. */
  const seamlessReconnect = async (label: string) => {
    toast.info(label);
    // Clear stale exit IP before changing connection
    useAppStore.setState({ exitIp: null });
    await toggleConnection();
    await new Promise((r) => setTimeout(r, 500));
    await toggleConnection();
  };

  const handleToggleConnection = async () => {
    await toggleConnection();
    const freshStatus = useAppStore.getState().status;
    const freshActiveServer = useAppStore.getState().activeServer;
    if (freshStatus === 'disconnected') {
      toast(t('protectionDisabled'), { id: 'protection-disabled' });
    } else if (freshStatus === 'connected') {
      toast.success(
        `${t('connectedTo')}${getServerDisplayName(
          {
            countryName: freshActiveServer?.countryName,
            flag: freshActiveServer?.flag,
            name: freshActiveServer?.name,
            server: freshActiveServer?.server,
            port: freshActiveServer?.port,
          },
          i18n.language,
        )}`,
        {
          description: t('trafficProtected'),
        },
      );
    }
    // status === 'error' toast is already shown by store.ts toggleConnection
  };

  const isConnected = status === 'connected';
  const isConnecting = status === 'connecting' || status === 'disconnecting';
  const isButtonDisabled = status === 'disconnecting';
  const isCancellable = status === 'connecting';

  const sparklinePath = (): string => {
    const data = sparklineData;
    if (data.length < 2) return '';
    const w = 120,
      h = 30;
    const maxVal = Math.max(...data, 1);
    const pts = data.map(
      (_v, i) => `${String((i / (data.length - 1)) * w)},${String(h - (_v / maxVal) * h)}`,
    );
    return `M${pts.join(' L')}`;
  };

  return (
    <div className="flex flex-col h-full w-full p-10 flex-1">
      <header className="flex justify-between items-center mb-6">
        <div>
          <h1 className="text-3xl font-bold tracking-tight text-white">OmniTun</h1>
          <p className="text-zinc-400 text-sm mt-1">{t('secureTunnelSubtitle')}</p>
        </div>
      </header>

      <div className="flex-1 flex flex-row gap-8 lg:gap-12 items-start justify-center w-full mx-auto">
        {servers.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-5 bg-zinc-900/30 border border-white/5 p-12 rounded-3xl backdrop-blur-sm w-full max-w-lg">
            <div className="w-20 h-20 bg-white/5 rounded-2xl flex items-center justify-center shadow-inner">
              {isAnyProfileRefreshing ? (
                <Loader2 size={40} className="text-emerald-500 animate-spin" strokeWidth={1.5} />
              ) : (
                <Server size={40} className="text-zinc-500" />
              )}
            </div>
            <div className="text-center">
              <p className="text-zinc-200 font-semibold text-lg">
                {isAnyProfileRefreshing ? t('updatingProfile').trim() : t('noServersTitle')}
              </p>
              <p className="text-zinc-500 text-sm mt-1">
                {isAnyProfileRefreshing
                  ? t('testingServersDesc')
                  : t('noServersDesc')}
              </p>
            </div>
            {!isAnyProfileRefreshing && (
              <button
                onClick={() => onNavigate?.('profiles')}
                className="mt-4 bg-emerald-500 hover:bg-emerald-400 text-zinc-950 font-bold rounded-xl px-6 py-3 flex items-center gap-2 transition-all active:scale-95 shadow-[0_0_20px_rgba(16,185,129,0.3)]"
              >
                <Plus size={18} strokeWidth={2.5} />
                {t('addSubscription')}
              </button>
            )}
          </div>
        ) : (
          <>
            {/* Left Column: Controls & Power Button */}
            <div className="flex flex-col items-center flex-1 max-w-sm w-full">
              <div className="w-full flex flex-col items-center gap-6 mb-6 z-20">
                {/* Server Picker */}
                <div className="relative w-full z-30">
                  <motion.button
                    whileHover={{ scale: settings.autoConnectFastest ? 1 : 1.02 }}
                    whileTap={{ scale: settings.autoConnectFastest ? 1 : 0.98 }}
                    onClick={() => {
                      if (settings.autoConnectFastest) {
                        toast.info(
                          t('smartNodeDesc') ||
                            'Manual selection is disabled while Smart Node is active',
                        );
                        return;
                      }
                      setIsServerDropdownOpen(!isServerDropdownOpen);
                    }}
                    className={`w-full px-5 py-3 h-13 bg-zinc-900/60 hover:bg-zinc-800/80 backdrop-blur-xl transition-all rounded-2xl border border-white/10 flex items-center justify-between gap-3 shadow-lg ${settings.autoConnectFastest ? 'opacity-80 cursor-default' : ''}`}
                  >
                    <div className="flex items-center gap-3 overflow-hidden w-full min-w-0">
                      <div className="flex items-center gap-1 shrink-0 h-7">
                        {activeServer?.exitFlag &&
                        activeServer.flag &&
                        activeServer.exitServer !== activeServer.server &&
                        activeServer.exitFlag !== activeServer.flag ? (
                          <div className="flex items-center gap-0.5 text-lg text-zinc-500">
                            <FlagDisplay code={activeServer.flag} className="text-xs" />
                            <span>→</span>
                            <FlagDisplay code={activeServer.exitFlag} className="text-xs" />
                          </div>
                        ) : (
                          <FlagDisplay
                            code={activeServer?.exitFlag ?? activeServer?.flag}
                            className="text-lg"
                          />
                        )}
                      </div>

                      <div className="flex flex-col items-start overflow-hidden min-w-0 flex-1">
                        <div className="flex items-center gap-1.5 w-full">
                          <span className="text-sm font-medium leading-tight truncate">
                            {getServerDisplayName(
                              {
                                countryName:
                                  activeServer?.exitCountryName ?? activeServer?.countryName,
                                flag: activeServer?.exitFlag ?? activeServer?.flag,
                                name: activeServer?.name,
                                server: activeServer?.server,
                                port: activeServer?.port,
                              },
                              i18n.language,
                            )}
                          </span>
                          {activeServer?.behindCdn && (
                            <span className="text-[10px] bg-amber-500/15 text-amber-400 px-1.5 py-0.5 rounded font-mono shrink-0">
                              CDN
                            </span>
                          )}
                        </div>
                      </div>
                    </div>

                    <div className="flex items-center gap-2 shrink-0 pl-3 border-l border-white/10">
                      <div
                        className={`w-2 h-2 rounded-full ${isConnected ? 'bg-emerald-500 shadow-[0_0_10px_rgba(16,185,129,0.8)]' : 'bg-zinc-600'}`}
                      />
                      <span className="text-xs font-mono text-zinc-400 w-12 text-right truncate">
                        {activeServer?.ping ?? '-'}ms
                      </span>
                      <ChevronDown
                        size={16}
                        className={`text-zinc-500 transition-transform duration-300 ${isServerDropdownOpen ? 'rotate-180' : ''}`}
                      />
                    </div>
                  </motion.button>

                  <AnimatePresence>
                    {isServerDropdownOpen && (
                      <>
                        <div
                          className="fixed inset-0 z-10"
                          onClick={() => {
                            setIsServerDropdownOpen(false);
                          }}
                        />
                        <motion.div
                          initial={{ opacity: 0, y: 10, scale: 0.95 }}
                          animate={{ opacity: 1, y: 0, scale: 1 }}
                          exit={{ opacity: 0, y: 10, scale: 0.95 }}
                          transition={{ duration: 0.2, ease: 'easeOut' }}
                          className="absolute top-full left-0 right-0 mt-3 w-full bg-zinc-900/95 backdrop-blur-2xl border border-white/10 rounded-2xl shadow-2xl p-2 z-20 max-h-72 overflow-y-auto custom-scrollbar"
                        >
                          <p className="text-[10px] font-bold text-zinc-500 uppercase tracking-widest px-3 py-2 mb-1">
                            {t('availableEndpoints')} ({servers.length})
                          </p>
                          <div className="flex flex-col gap-1">
                            {servers.slice(0, MAX_DROPDOWN_SERVERS).map((server) => (
                              <button
                                key={server.id}
                                onClick={() => {
                                  const localizedName = getServerDisplayName(
                                    {
                                      countryName: server.exitCountryName ?? server.countryName,
                                      flag: server.exitFlag ?? server.flag,
                                      name: server.name,
                                      server: server.server,
                                      port: server.port,
                                    },
                                    i18n.language,
                                  );

                                  setActiveServer(server);
                                  setIsServerDropdownOpen(false);
                                  addLog(`Server changed to ${server.name}`, 'info', 'system');

                                  // Auto-reconnect if already connected
                                  if (status === 'connected') {
                                    void seamlessReconnect(
                                      t('reconnectingTo', { name: localizedName }),
                                    );
                                  } else {
                                    toast.success(
                                      `${t('serverStr')}${localizedName}${t('selectedStr')}`,
                                    );
                                  }
                                }}
                                disabled={status === 'connecting'}
                                className={`flex items-center justify-between p-2.5 min-h-11 rounded-xl transition-all w-full text-left ${
                                  activeServer?.id === server.id
                                    ? 'bg-emerald-500/15 text-emerald-400 border border-emerald-500/20'
                                    : 'hover:bg-white/5 text-zinc-300 border border-transparent'
                                }`}
                              >
                                <div className="flex items-center gap-3 overflow-hidden w-full min-w-0">
                                  <div className="flex items-center gap-1 shrink-0 h-7">
                                    {server.exitFlag &&
                                    server.flag &&
                                    server.exitServer !== server.server &&
                                    server.exitFlag !== server.flag ? (
                                      <div className="flex items-center gap-0.5 text-lg text-zinc-500">
                                        <FlagDisplay code={server.flag} className="text-xs" />
                                        <span>→</span>
                                        <FlagDisplay code={server.exitFlag} className="text-xs" />
                                      </div>
                                    ) : (
                                      <FlagDisplay
                                        code={server.exitFlag ?? server.flag}
                                        className="text-lg"
                                      />
                                    )}
                                  </div>

                                  <div className="flex flex-col items-start overflow-hidden min-w-0 flex-1">
                                    <div className="flex items-center gap-1.5 w-full">
                                      <span className="text-sm font-medium leading-tight truncate">
                                        {getServerDisplayName(
                                          {
                                            countryName:
                                              server.exitCountryName ?? server.countryName,
                                            flag: server.exitFlag ?? server.flag,
                                            name: server.name,
                                            server: server.server,
                                            port: server.port,
                                          },
                                          i18n.language,
                                        )}
                                      </span>
                                      {server.behindCdn && (
                                        <span className="text-[10px] bg-amber-500/15 text-amber-400 px-1.5 py-0.5 rounded font-mono shrink-0">
                                          CDN
                                        </span>
                                      )}
                                    </div>
                                  </div>
                                </div>
                                <div className="flex items-center gap-2 shrink-0 pl-2">
                                  <span
                                    className={`text-[10px] font-mono px-2 py-0.5 rounded-md ${(server.ping ?? 999) < 60 ? 'bg-emerald-500/10 text-emerald-400' : 'bg-yellow-500/10 text-yellow-400'}`}
                                  >
                                    {server.ping ?? '-'}ms
                                  </span>
                                  {activeServer?.id === server.id && (
                                    <Check size={14} strokeWidth={3} className="text-emerald-400" />
                                  )}
                                </div>
                              </button>
                            ))}
                          </div>
                          {servers.length > MAX_DROPDOWN_SERVERS && (
                            <p className="text-[10px] text-zinc-600 text-center py-2 px-3">
                              Showing top {String(MAX_DROPDOWN_SERVERS)} of {String(servers.length)}{' '}
                              (sorted by ping)
                            </p>
                          )}
                        </motion.div>
                      </>
                    )}
                  </AnimatePresence>
                </div>

                {/* Mode Selector */}
                <div className="w-full flex flex-col items-center z-20">
                  <SegmentedControl
                    options={[
                      { id: 'system-proxy', label: t('systemProxyMode') },
                      { id: 'local-proxy', label: t('proxyMode') },
                    ]}
                    value={settings.routingMode}
                    onChange={(val) => {
                      if (val === settings.routingMode) return;

                      updateSettings({ routingMode: val });
                      addLog(`Routing mode changed to ${val}`, 'info', 'settings');

                      if (status === 'connected') {
                        // Connected: seamless stop + start with new mode
                        void seamlessReconnect(
                          val === 'system-proxy' ? t('systemProxyMode') : t('proxyMode'),
                        );
                      } else if (status === 'error') {
                        // Error state: restart proxy with new mode
                        void toggleConnection();
                      }
                    }}
                    disabled={false}
                    layoutId="modeIndicatorDashboard"
                  />

                  {/* Fixed height container to prevent layout shifts */}
                  <div className="h-14 mt-3 relative w-full flex justify-center">
                    <AnimatePresence mode="wait" initial={false}>
                      <motion.div
                        key={settings.routingMode}
                        initial={{ opacity: 0, y: 5 }}
                        animate={{ opacity: 1, y: 0 }}
                        exit={{ opacity: 0, y: -5 }}
                        transition={{ duration: 0.2 }}
                        className="absolute inset-0 flex flex-col items-center justify-start text-center"
                      >
                        <p className="text-xs text-zinc-400 font-medium">
                          {settings.routingMode === 'system-proxy'
                            ? t('systemProxyModeDesc')
                            : t('proxyModeDesc')}
                        </p>
                        {/* Proxy port is always shown — both routing modes use a proxy port */}
                        <motion.div className="flex items-center justify-between gap-3 mt-2 bg-zinc-900/80 w-37.5 mx-auto rounded-xl border border-white/10 p-1.5 px-4 transition-all shadow-inner focus-within:ring-2 focus-within:ring-emerald-500/30 focus-within:border-emerald-500/50">
                          <span className="text-[10px] text-zinc-500 font-semibold tracking-wide uppercase">
                            {t('proxyPort')}
                          </span>
                          <input
                            type="number"
                            value={settings.proxyPort || 1080}
                            onChange={(e) => {
                              updateSettings({ proxyPort: Number(e.target.value) });
                            }}
                            disabled={isConnected || isConnecting}
                            className="bg-transparent text-xs tracking-widest text-emerald-400 w-12 focus:outline-none font-mono text-right disabled:opacity-50"
                          />
                        </motion.div>
                      </motion.div>
                    </AnimatePresence>
                  </div>
                </div>
              </div>

              {/* Connection button */}
              <div className="relative flex items-center justify-center my-2 z-10">
                {isConnected && (
                  <>
                    <motion.div
                      initial={{ scale: 0.8, opacity: 0 }}
                      animate={{ scale: 1.35, opacity: 0 }}
                      transition={{ repeat: Infinity, duration: 2.5, ease: 'easeOut' }}
                      className="absolute inset-0 rounded-full border border-emerald-500/30 bg-emerald-500/5"
                    />
                    <motion.div
                      initial={{ scale: 0.8, opacity: 0 }}
                      animate={{ scale: 1.2, opacity: 0 }}
                      transition={{ repeat: Infinity, duration: 2.5, delay: 0.8, ease: 'easeOut' }}
                      className="absolute inset-0 rounded-full border border-emerald-500/40"
                    />
                  </>
                )}

                <motion.button
                  whileHover={{ scale: 1.03 }}
                  whileTap={{ scale: 0.95 }}
                  onClick={() => void handleToggleConnection()}
                  disabled={isButtonDisabled}
                  className={`relative z-10 w-44 h-44 rounded-full flex flex-col items-center justify-center transition-all duration-500 group
                    ${isConnected ? 'bg-linear-to-b from-zinc-800 to-zinc-900 border-2 border-emerald-500/80 shadow-[0_0_50px_-10px_rgba(16,185,129,0.4)]' : 'bg-linear-to-b from-zinc-800 to-zinc-900 border-2 border-white/5 shadow-2xl hover:border-white/10'}
                    ${isConnecting ? 'animate-pulse border-blue-500/80 shadow-[0_0_40px_-10px_rgba(59,130,246,0.4)]' : ''}
                    ${status === 'error' ? 'border-red-500/80 shadow-[0_0_40px_-10px_rgba(239,68,68,0.4)]' : ''}
                    ${isCancellable ? 'cursor-pointer hover:border-blue-400/60' : ''}
                  `}
                >
                  <div
                    className={`absolute inset-0 rounded-full bg-linear-to-tr opacity-20 transition-opacity duration-500 ${isConnected ? 'from-emerald-500 to-transparent' : 'from-white/5 to-transparent'}`}
                  />
                  {isConnected ? (
                    <>
                      <Power
                        size={28}
                        strokeWidth={1.5}
                        className="mb-1 relative z-10 text-emerald-400 drop-shadow-[0_0_8px_rgba(16,185,129,0.8)]"
                      />
                      <p className="text-xl font-light font-mono tracking-tight text-white drop-shadow-sm relative z-10">
                        {sessionDuration}
                      </p>
                      <span className="text-[8px] text-emerald-500/80 font-bold uppercase tracking-[0.2em] relative z-10">
                        {t('activeSession')}
                      </span>
                    </>
                  ) : (
                    <>
                      <Power
                        size={48}
                        strokeWidth={1.5}
                        className={`mb-2 relative z-10 transition-colors duration-500 ${isConnecting ? 'text-blue-400' : status === 'error' ? 'text-red-400' : 'text-zinc-400 group-hover:text-zinc-200'}`}
                      />
                      <span
                        className={`text-[10px] font-bold uppercase tracking-[0.2em] relative z-10 transition-colors duration-500 ${isConnecting ? 'text-blue-400' : status === 'error' ? 'text-red-400' : 'text-zinc-500 group-hover:text-zinc-300'}`}
                      >
                        {status === 'error'
                          ? t('statusError')
                          : status === 'disconnecting'
                            ? t('disconnecting')
                            : status === 'connecting'
                              ? t('connecting')
                              : t('disconnected')}
                      </span>
                      {isCancellable && (
                        <span className="text-[8px] text-zinc-500 transition-colors duration-300 relative z-10 mt-1">
                          {t('clickToCancel')}
                        </span>
                      )}
                    </>
                  )}
                </motion.button>
              </div>

              {/* Outlet IP (Fixed Height Container) */}
              <div className="mt-6 text-center h-16 z-10 w-full flex flex-col items-center justify-start">
                <AnimatePresence mode="wait">
                  {isConnected ? (
                    <motion.div
                      key="connected-info"
                      initial={{ opacity: 0, y: 10 }}
                      animate={{ opacity: 1, y: 0 }}
                      exit={{ opacity: 0, y: -10 }}
                      className="flex flex-col items-center text-center w-full"
                    >
                      {/* Neatly grouped IP Pill */}
                      {activeServer && (
                        <div className="flex flex-col items-center bg-zinc-900/30 border border-white/5 rounded-xl px-4 py-2 shadow-inner w-full">
                          {/* Row 1: country info */}
                          <div className="flex items-center gap-1.5 text-xs text-zinc-300 font-medium min-w-0 w-full justify-center">
                            {!activeServer.behindCdn &&
                            activeServer.exitFlag &&
                            activeServer.exitFlag === activeServer.flag &&
                            (exitIp ?? activeServer.exitServer) ? (
                              // Same entry/exit country — show exit data directly
                              <>
                                <Globe size={12} className="text-emerald-500 shrink-0" />
                                {(exitIp?.flag ?? activeServer.exitFlag) && (
                                  <FlagDisplay
                                    code={exitIp?.flag ?? activeServer.exitFlag}
                                    className="text-xs shrink-0"
                                  />
                                )}
                                <span className="truncate">
                                  {getExitIpDisplayName(
                                    exitIp ?? {
                                      server: activeServer.exitServer,
                                      flag: activeServer.exitFlag,
                                      country_name: activeServer.exitCountryName,
                                    },
                                    i18n.language,
                                  )}
                                </span>
                              </>
                            ) : (
                              // Different countries or no exit — show entry
                              <>
                                <Globe size={12} className="text-emerald-500 shrink-0" />
                                {activeServer.flag && (
                                  <FlagDisplay
                                    code={activeServer.flag}
                                    className="text-xs shrink-0"
                                  />
                                )}
                                <span className="truncate">
                                  {getServerDisplayName(
                                    {
                                      countryName: activeServer.countryName,
                                      flag: activeServer.flag,
                                      name: activeServer.name,
                                      server: activeServer.server,
                                      port: activeServer.port,
                                    },
                                    i18n.language,
                                  )}
                                </span>
                                {activeServer.behindCdn && (
                                  <span className="text-[10px] bg-amber-500/15 text-amber-400 px-1.5 py-0.5 rounded shrink-0 font-mono">
                                    CDN
                                  </span>
                                )}
                                {!activeServer.behindCdn && (exitIp ?? activeServer.exitServer) && (
                                  <>
                                    <span className="text-zinc-600 shrink-0">→</span>
                                    {(exitIp?.flag ?? activeServer.exitFlag) && (
                                      <FlagDisplay
                                        code={exitIp?.flag ?? activeServer.exitFlag}
                                        className="text-xs shrink-0"
                                      />
                                    )}
                                    <span className="truncate">
                                      {getExitIpDisplayName(
                                        exitIp ?? {
                                          server: activeServer.exitServer,
                                          flag: activeServer.exitFlag,
                                          country_name: activeServer.exitCountryName,
                                        },
                                        i18n.language,
                                      )}
                                    </span>
                                  </>
                                )}
                              </>
                            )}
                          </div>
                          {/* Row 2: IP addresses */}
                          <div className="flex items-center gap-1.5 text-[11px] text-zinc-500 font-mono min-w-0 w-full justify-center mt-0.5">
                            {!activeServer.behindCdn &&
                            activeServer.exitFlag &&
                            activeServer.exitFlag === activeServer.flag &&
                            (exitIp ?? activeServer.exitServer) ? (
                              <span className="truncate">
                                {exitIp?.server ?? activeServer.exitServer}
                              </span>
                            ) : (
                              <>
                                <span className="truncate">{activeServer.server}</span>
                                {!activeServer.behindCdn &&
                                  (exitIp ?? activeServer.exitServer) &&
                                  (exitIp?.server ?? activeServer.exitServer) !==
                                    activeServer.server && (
                                    <>
                                      <span className="text-zinc-700 shrink-0">·</span>
                                      <span className="truncate">
                                        {exitIp?.server ?? activeServer.exitServer}
                                      </span>
                                    </>
                                  )}
                              </>
                            )}
                          </div>
                        </div>
                      )}
                    </motion.div>
                  ) : realIp ? (
                    <motion.div
                      key="real-ip-info"
                      initial={{ opacity: 0, y: 10 }}
                      animate={{ opacity: 1, y: 0 }}
                      exit={{ opacity: 0, y: -10 }}
                      className="flex flex-col items-center text-center w-full"
                    >
                      <div className="flex flex-col items-center bg-zinc-900/30 border border-white/5 rounded-xl px-4 py-2 shadow-inner w-full">
                        <div className="flex items-center gap-1.5 text-xs text-zinc-300 font-medium min-w-0 w-full justify-center">
                          <Globe size={12} className="text-zinc-500 shrink-0" />
                          {realIp.flag && (
                            <FlagDisplay code={realIp.flag} className="text-xs shrink-0" />
                          )}
                          <span className="truncate">
                            {getExitIpDisplayName(
                              {
                                country_name: realIp.country_name,
                                flag: realIp.flag,
                                server: realIp.server,
                              },
                              i18n.language,
                            )}
                          </span>
                        </div>
                        <span className="text-[11px] text-zinc-500 font-mono mt-0.5">
                          {realIp.server}
                        </span>
                      </div>
                    </motion.div>
                  ) : null}
                </AnimatePresence>
              </div>
            </div>

            {/* Right Column: Stats & Info */}
            <div className="w-80 shrink-0 flex flex-col gap-5 z-10">
              {/* Protocol status */}
              <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-6 shadow-lg">
                <p className="text-zinc-500 text-[10px] font-bold uppercase tracking-widest mb-3">
                  {t('protocolStatus')}
                </p>
                <div className="h-6 flex items-center justify-between">
                  <span className="text-sm font-semibold text-zinc-200 truncate pr-2 min-w-0">
                    {isConnected
                      ? activeServer?.protocol
                        ? formatProtocol(activeServer.protocol)
                        : 'Unknown'
                      : t('standby')}
                  </span>
                  <span
                    className={`text-xs font-mono px-2 py-1 rounded-md shrink-0 ${isConnected ? 'bg-emerald-500/10 text-emerald-400' : 'bg-zinc-800 text-zinc-500'}`}
                  >
                    {isConnected ? t('stable') : t('idle')}
                  </span>
                </div>
              </div>

              {/* Traffic + sparkline */}
              <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-6 shadow-lg">
                <p className="text-zinc-500 text-[10px] font-bold uppercase tracking-widest mb-3">
                  {t('networkTraffic')}
                </p>
                <div className="h-21 flex flex-col justify-end">
                  {isConnected ? (
                    <>
                      <div className="flex justify-between items-end gap-3">
                        <div className="flex flex-col w-24 min-w-0">
                          <span className="text-emerald-400 font-mono text-xl font-medium tracking-tight truncate">
                            {formatSpeed(speeds.down).value}
                          </span>
                          <span className="text-[10px] text-zinc-500 uppercase tracking-wider mt-0.5 truncate">
                            {formatSpeed(speeds.down).unit} {t('down')}
                          </span>
                        </div>
                        <div className="flex flex-col items-end w-24 min-w-0">
                          <span className="text-blue-400 font-mono text-xl font-medium tracking-tight truncate">
                            {formatSpeed(speeds.up).value}
                          </span>
                          <span className="text-[10px] text-zinc-500 uppercase tracking-wider mt-0.5 truncate">
                            {formatSpeed(speeds.up).unit} {t('up')}
                          </span>
                        </div>
                      </div>
                      {/* Sparkline */}
                      {sparklineData.length >= 2 && (
                        <div className="mt-2 flex justify-center opacity-60 h-7.5">
                          <svg
                            width="120"
                            height="30"
                            viewBox="0 0 120 30"
                            className="overflow-visible"
                          >
                            <path
                              d={sparklinePath()}
                              fill="none"
                              stroke="#10b981"
                              strokeWidth="1.5"
                              strokeLinecap="round"
                              strokeLinejoin="round"
                            />
                          </svg>
                        </div>
                      )}
                    </>
                  ) : (
                    <div className="h-full flex items-center justify-center">
                      <span className="text-sm text-zinc-600 font-medium">
                        {t('connectToMonitor')}
                      </span>
                    </div>
                  )}
                </div>
              </div>

              {/* Routing + speed test */}
              <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-6 shadow-lg">
                <p className="text-zinc-500 text-[10px] font-bold uppercase tracking-widest mb-3">
                  {t('smartRouting')}
                </p>
                <div className="h-19 flex flex-col justify-between">
                  <div className="flex items-center justify-between">
                    <div className="flex items-center gap-3 overflow-hidden min-w-0">
                      <div
                        className={`w-2.5 h-2.5 rounded-full shrink-0 ${isConnected ? 'bg-emerald-500 shadow-[0_0_8px_rgba(16,185,129,0.6)]' : 'bg-zinc-700'}`}
                      />
                      <span className="text-sm text-zinc-300 font-medium truncate">
                        {isConnected ? t('bypassingLocal') : t('routingDisabled')}
                      </span>
                    </div>
                    {/* Kill switch indicator */}
                    {isConnected && settings.killSwitch && (
                      <span className="text-[10px] text-emerald-500/80 font-bold uppercase tracking-wider shrink-0 ml-2">
                        {t('enabled')}
                      </span>
                    )}
                  </div>

                  {/* Speed test button (or placeholder to maintain height) */}
                  <AnimatePresence mode="wait">
                    {isConnected ? (
                      <motion.button
                        initial={{ opacity: 0 }}
                        animate={{ opacity: 1 }}
                        exit={{ opacity: 0 }}
                        onClick={() => void handleSpeedTest()}
                        disabled={isSpeedTesting}
                        className="flex items-center justify-center gap-2 bg-emerald-500/10 hover:bg-emerald-500/20 text-emerald-400 rounded-xl py-2.5 transition-all text-xs font-semibold disabled:opacity-50 w-full overflow-hidden"
                      >
                        <Gauge
                          size={14}
                          className={isSpeedTesting ? 'animate-spin shrink-0' : 'shrink-0'}
                        />
                        <span className="truncate">
                          {isSpeedTesting
                            ? t('speedTesting')
                            : speedTestResult?.delay
                              ? `${t('speedTestResult')}: ${String(speedTestResult.delay)} ms`
                              : t('speedTest')}
                        </span>
                      </motion.button>
                    ) : (
                      <div className="h-9" /> // Placeholder
                    )}
                  </AnimatePresence>
                </div>
              </div>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
