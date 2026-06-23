import { useEffect, useRef, useState } from 'react';
import { AnimatePresence, motion } from 'framer-motion';
import { Check, ChevronDown, Gauge, Globe, Plus, Power, Server } from 'lucide-react';
import { toast } from 'sonner';
import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { SegmentedControl } from '../SegmentedControl';

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

type Tab = 'home' | 'profiles' | 'settings' | 'logs' | 'about';

export function Dashboard({ onNavigate }: { readonly onNavigate?: (tab: Tab) => void }) {
  const {
    status,
    activeServer,
    servers,
    speeds,
    connectedAt,
    setSpeeds,
    setActiveServer,
    settings,
    updateSettings,
    outletIp,
    fetchOutletIp,
    realIp,
    speedTestResult,
    runSpeedTest,
  } = useVPNStore();
  const toggleConnection = useVPNStore((state) => state.toggleConnection);
  const addLog = useVPNStore((state) => state.addLog);
  const { t } = useTranslation();

  const formatElapsed = (since: number): string => {
    const diff = Math.floor((Date.now() - since) / 1000);
    const h = Math.floor(diff / 3600).toString().padStart(2, '0');
    const m = Math.floor((diff % 3600) / 60).toString().padStart(2, '0');
    const s = (diff % 60).toString().padStart(2, '0');
    return `${h}:${m}:${s}`;
  };

  const [isServerDropdownOpen, setIsServerDropdownOpen] = useState(false);
  const [isSpeedTesting, setIsSpeedTesting] = useState(false);
  const [sessionDuration, setSessionDuration] = useState<string>(() =>
    connectedAt ? formatElapsed(connectedAt) : '00:00:00',
  );

  // Speed sparkline — ring buffer of last 30 values
  const [sparklineData, setSparklineData] = useState<number[]>([]);
  const sparklineRef = useRef(sparklineData);

  // Keep ref in sync without writing during render
  useEffect(() => {
    sparklineRef.current = sparklineData;
  }, [sparklineData]);

  // Session timer when connected
  useEffect(() => {
    if (status !== 'connected' || !connectedAt) return;
    const interval = setInterval(() => {
      setSessionDuration(formatElapsed(connectedAt));
    }, 1000);
    return () => { clearInterval(interval); };
  }, [status, connectedAt]);

  // Reset speeds display when disconnected
  useEffect(() => {
    if (status !== 'connected') {
      setSpeeds({ up: 0, down: 0 });
    }
  }, [status, setSpeeds]);

  // Fetch outlet IP when connected
  useEffect(() => {
    if (status === 'connected') {
      void fetchOutletIp();
      const ipInterval = setInterval(() => { void fetchOutletIp(); }, 30000);
      return () => { clearInterval(ipInterval); };
    }
  }, [status, fetchOutletIp]);

  // Feed sparkline from speeds — triggered by traffic monitor updates
  // We read speeds directly from the store inside setSparklineData to avoid deps issues
  useEffect(() => {
    if (status !== 'connected') return;
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setSparklineData((prev) => {
      const s = useVPNStore.getState().speeds;
      const total = s.down + s.up;
      const next = [...prev, total];
      return next.length > 30 ? next.slice(next.length - 30) : next;
    });
    const interval = setInterval(() => {
      setSparklineData((prev) => {
        const s = useVPNStore.getState().speeds;
        const total = s.down + s.up;
        const next = [...prev, total];
        return next.length > 30 ? next.slice(next.length - 30) : next;
      });
    }, 1000);
    return () => { clearInterval(interval); };
  }, [status]);

  const handleSpeedTest = async () => {
    setIsSpeedTesting(true);
    try {
      const result = await runSpeedTest();
      if (result?.delay) {
        toast.success(`Latency: ${String(result.delay)} ms`);
      } else {
        toast.error('Speed test failed');
      }
    } catch {
      toast.error('Speed test failed');
    } finally {
      setIsSpeedTesting(false);
    }
  };

  const handleToggleConnection = async () => {
    try {
      await toggleConnection();
      if (status === 'disconnected' || status === 'error') {
        toast.success(`${t('connectedTo')}${activeServer?.name ?? ''}`, {
          description: t('trafficProtected'),
        });
      } else {
        toast(t('protectionDisabled'));
      }
    } catch (e: unknown) {
      const msg = typeof e === 'string' ? e : e instanceof Error ? e.message : t('statusError');
      addLog(`Connection error: ${msg}`, 'error', 'system');
      toast.error(msg, { description: t('connectionFailedDesc') });
    }
  };

  const isConnected = status === 'connected';
  const isConnecting = status === 'connecting' || status === 'disconnecting';

  // SVG sparkline path
  const sparklinePath = (): string => {
    const data = sparklineData;
    if (data.length < 2) return '';
    const w = 120, h = 30;
    const maxVal = Math.max(...data, 1);
    const pts = data.map((_v, i) => `${String((i / (data.length - 1)) * w)},${String(h - (_v / maxVal) * h)}`);
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

      <div className="flex-1 flex gap-12 items-center justify-center">
        {servers.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-5 bg-zinc-900/30 border border-white/5 p-12 rounded-3xl backdrop-blur-sm">
            <div className="w-20 h-20 bg-white/5 rounded-2xl flex items-center justify-center shadow-inner">
              <Server size={40} className="text-zinc-500" />
            </div>
            <div className="text-center">
              <p className="text-zinc-200 font-semibold text-lg">{t('noServersTitle')}</p>
              <p className="text-zinc-500 text-sm mt-1">{t('noServersDesc')}</p>
            </div>
            <button
              onClick={() => onNavigate?.('profiles')}
              className="mt-4 bg-emerald-500 hover:bg-emerald-400 text-zinc-950 font-bold rounded-xl px-6 py-3 flex items-center gap-2 transition-all active:scale-95 shadow-[0_0_20px_rgba(16,185,129,0.3)]"
            >
              <Plus size={18} strokeWidth={2.5} />
              {t('addSubscription')}
            </button>
          </div>
        ) : (
          <>
            <div className="flex flex-col items-center justify-center">
              {/* Mode selector & server picker */}
              <div className="mb-8 flex items-start gap-6">
                <div className="w-72 flex flex-col items-center">
                  <SegmentedControl
                    options={[
                      { id: 'vpn', label: t('vpnMode') },
                      { id: 'system-proxy', label: t('systemProxyMode') },
                      { id: 'local-proxy', label: t('proxyMode') },
                    ]}
                    value={settings.routingMode}
                    onChange={(val) => {
                      updateSettings({ routingMode: val });
                      addLog(`Routing mode changed to ${val}`, 'info', 'settings');
                    }}
                    disabled={isConnected || isConnecting}
                    layoutId="modeIndicatorDashboard"
                  />
                  <div className="h-20 mt-5 relative w-full">
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
                          {settings.routingMode === 'vpn'
                            ? t('vpnModeDesc')
                            : settings.routingMode === 'system-proxy'
                              ? t('systemProxyModeDesc')
                              : t('proxyModeDesc')}
                        </p>
                        {(settings.routingMode === 'system-proxy' || settings.routingMode === 'local-proxy') && (
                          <motion.div className="flex items-center justify-between gap-3 mt-3 bg-zinc-900/80 w-37.5 rounded-xl border border-white/10 p-2 px-4 transition-all shadow-inner focus-within:ring-2 focus-within:ring-emerald-500/30 focus-within:border-emerald-500/50">
                            <span className="text-xs text-zinc-500 font-semibold tracking-wide uppercase">
                              {t('proxyPort')}
                            </span>
                            <input
                              type="number"
                              value={settings.proxyPort || 1080}
                              onChange={(e) => { updateSettings({ proxyPort: Number(e.target.value) }); }}
                              disabled={isConnected || isConnecting}
                              className="bg-transparent text-sm tracking-widest text-emerald-400 w-14 focus:outline-none font-mono text-right disabled:opacity-50"
                            />
                          </motion.div>
                        )}
                      </motion.div>
                    </AnimatePresence>
                  </div>
                </div>

                <div className="relative">
                  <motion.button
                    whileHover={{ scale: 1.02 }}
                    whileTap={{ scale: 0.98 }}
                    onClick={() => { setIsServerDropdownOpen(!isServerDropdownOpen); }}
                    className="px-5 py-3 bg-zinc-900/60 hover:bg-zinc-800/80 backdrop-blur-xl transition-all rounded-2xl border border-white/10 flex items-center gap-3 z-20 shadow-lg"
                  >
                    <span className="text-xl drop-shadow-md">{activeServer?.flag}</span>
                    <span className="font-semibold text-sm text-zinc-100">
                      {activeServer?.name}
                    </span>
                    <div className="flex items-center gap-1.5 ml-3 pl-3 border-l border-white/10">
                      <div className={`w-2 h-2 rounded-full ${isConnected ? 'bg-emerald-500 shadow-[0_0_10px_rgba(16,185,129,0.8)]' : 'bg-zinc-600'}`} />
                      <span className="text-xs font-mono text-zinc-400 w-10 text-right">
                        {activeServer?.ping} ms
                      </span>
                    </div>
                    <ChevronDown size={16} className={`ml-1 text-zinc-500 transition-transform duration-300 ${isServerDropdownOpen ? 'rotate-180' : ''}`} />
                  </motion.button>

                  <AnimatePresence>
                    {isServerDropdownOpen && (
                      <>
                        <div className="fixed inset-0 z-10" onClick={() => { setIsServerDropdownOpen(false); }} />
                        <motion.div
                          initial={{ opacity: 0, y: 10, scale: 0.95 }}
                          animate={{ opacity: 1, y: 0, scale: 1 }}
                          exit={{ opacity: 0, y: 10, scale: 0.95 }}
                          transition={{ duration: 0.2, ease: 'easeOut' }}
                          className="absolute top-full right-0 mt-3 w-72 bg-zinc-900/95 backdrop-blur-2xl border border-white/10 rounded-2xl shadow-2xl p-2 z-20 max-h-72 overflow-y-auto custom-scrollbar"
                        >
                          <p className="text-[10px] font-bold text-zinc-500 uppercase tracking-widest px-3 py-2 mb-1">
                            {t('availableEndpoints')} ({servers.length})
                          </p>
                          <div className="flex flex-col gap-1">
                            {servers.map((server) => (
                              <button
                                key={server.id}
                                onClick={() => {
                                  setActiveServer(server);
                                  setIsServerDropdownOpen(false);
                                  addLog(`Server changed to ${server.name}`, 'info', 'system');
                                  toast.success(`${t('serverStr')}${server.name}${t('selectedStr')}`);
                                }}
                                disabled={status === 'connecting'}
                                className={`flex items-center justify-between p-2.5 rounded-xl transition-all ${
                                  activeServer?.id === server.id
                                    ? 'bg-emerald-500/15 text-emerald-400 border border-emerald-500/20'
                                    : 'hover:bg-white/5 text-zinc-300 border border-transparent'
                                }`}
                              >
                                <div className="flex items-center gap-3">
                                  <span className="text-lg">{server.flag}</span>
                                  <span className="text-sm font-medium">{server.name}</span>
                                </div>
                                <div className="flex items-center gap-2">
                                  <span className={`text-[10px] font-mono px-2 py-0.5 rounded-md ${(server.ping ?? 999) < 60 ? 'bg-emerald-500/10 text-emerald-400' : 'bg-yellow-500/10 text-yellow-400'}`}>
                                    {server.ping ?? '-'}ms
                                  </span>
                                  {activeServer?.id === server.id && <Check size={14} strokeWidth={3} />}
                                </div>
                              </button>
                            ))}
                          </div>
                        </motion.div>
                      </>
                    )}
                  </AnimatePresence>
                </div>
              </div>

              {/* Connection button */}
              <div className="relative flex items-center justify-center my-3">
                {isConnected && (
                  <>
                    <motion.div
                      initial={{ scale: 0.8, opacity: 0 }}
                      animate={{ scale: 1.5, opacity: 0 }}
                      transition={{ repeat: Infinity, duration: 2.5, ease: 'easeOut' }}
                      className="absolute inset-0 rounded-full border border-emerald-500/30 bg-emerald-500/5"
                    />
                    <motion.div
                      initial={{ scale: 0.8, opacity: 0 }}
                      animate={{ scale: 1.3, opacity: 0 }}
                      transition={{ repeat: Infinity, duration: 2.5, delay: 0.8, ease: 'easeOut' }}
                      className="absolute inset-0 rounded-full border border-emerald-500/40"
                    />
                  </>
                )}

                <motion.button
                  whileHover={{ scale: 1.03 }}
                  whileTap={{ scale: 0.95 }}
                  onClick={() => void handleToggleConnection()}
                  disabled={isConnecting}
                  className={`relative z-10 w-44 h-44 rounded-full flex flex-col items-center justify-center transition-all duration-500 group border-4
                    ${isConnected ? 'bg-zinc-900 border-emerald-500 shadow-[0_0_50px_rgba(16,185,129,0.2)]' : 'bg-zinc-900 border-zinc-800 shadow-[0_0_30px_rgba(0,0,0,0.5)]'}
                    ${isConnecting ? 'animate-pulse border-blue-500 shadow-[0_0_30px_rgba(59,130,246,0.3)]' : ''}
                    ${status === 'error' ? 'border-red-500 shadow-[0_0_30px_rgba(239,68,68,0.3)]' : ''}`}
                >
                  <Power size={48} strokeWidth={1.5}
                    className={`mb-2 relative z-10 transition-colors duration-500 ${isConnected ? 'text-emerald-500' : isConnecting ? 'text-blue-500' : status === 'error' ? 'text-red-500' : 'text-zinc-500 group-hover:text-zinc-300'}`}
                  />
                  <span className={`text-[10px] font-bold uppercase tracking-[0.2em] relative z-10 transition-colors duration-500 ${isConnected ? 'text-emerald-500' : isConnecting ? 'text-blue-500' : status === 'error' ? 'text-red-500' : 'text-zinc-500 group-hover:text-zinc-300'}`}>
                    {status === 'error' ? t('statusError') : isConnected ? t('connected') : status === 'disconnecting' ? t('disconnecting') : status === 'connecting' ? t('connecting') : t('disconnected')}
                  </span>
                </motion.button>
              </div>

              {/* Session timer + outlet IP */}
              <div className="mt-6 text-center h-14">
                {isConnected && (
                  <motion.div initial={{ opacity: 0, y: 10 }} animate={{ opacity: 1, y: 0 }} className="flex flex-col items-center text-center">
                    <p className="text-3xl font-light font-mono tracking-tight text-white drop-shadow-sm">
                      {sessionDuration}
                    </p>
                    <p className="text-emerald-500/80 text-[10px] uppercase tracking-[0.2em] mt-1 font-bold">
                      {t('activeSession')}
                    </p>
                    {outletIp && (
                      <p className="text-xs text-zinc-400 font-mono mt-2 flex items-center gap-1.5">
                        <Globe size={12} className="text-emerald-500" />
                        {outletIp}
                      </p>
                    )}
                  </motion.div>
                )}
                {realIp && status !== 'connected' && (
                  <motion.div initial={{ opacity: 0, y: 10 }} animate={{ opacity: 1, y: 0 }} className="flex flex-col items-center text-center mt-4">
                    <p className="text-xs text-zinc-500 font-mono flex items-center gap-1.5">
                      <Globe size={12} className="text-zinc-500" />
                      {realIp}
                    </p>
                  </motion.div>
                )}
              </div>
            </div>

            {/* Right panel: protocol, traffic, routing, extra */}
            <div className="w-80 flex flex-col gap-4">
              {/* Protocol status */}
              <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-5 shadow-lg">
                <p className="text-zinc-500 text-[10px] font-bold uppercase tracking-widest mb-3">
                  {t('protocolStatus')}
                </p>
                <div className="flex items-center justify-between">
                  <span className="text-sm font-semibold text-zinc-200">
                    {isConnected ? (activeServer?.protocol ? formatProtocol(activeServer.protocol) : 'Unknown') : t('standby')}
                  </span>
                  <span className={`text-xs font-mono px-2 py-1 rounded-md ${isConnected ? 'bg-emerald-500/10 text-emerald-400' : 'bg-zinc-800 text-zinc-500'}`}>
                    {isConnected ? t('stable') : t('idle')}
                  </span>
                </div>
              </div>

              {/* Traffic + sparkline */}
              <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-5 shadow-lg">
                <p className="text-zinc-500 text-[10px] font-bold uppercase tracking-widest mb-3">
                  {t('networkTraffic')}
                </p>
                {isConnected ? (
                  <>
                    <div className="flex justify-between items-end gap-3">
                      <div className="flex flex-col">
                        <span className="text-emerald-400 font-mono text-xl font-medium tracking-tight">
                          {speeds.down.toFixed(1)}
                        </span>
                        <span className="text-[10px] text-zinc-500 uppercase tracking-wider mt-0.5">
                          MB/s {t('down')}
                        </span>
                      </div>
                      <div className="flex flex-col items-end">
                        <span className="text-blue-400 font-mono text-xl font-medium tracking-tight">
                          {speeds.up.toFixed(1)}
                        </span>
                        <span className="text-[10px] text-zinc-500 uppercase tracking-wider mt-0.5">
                          MB/s {t('up')}
                        </span>
                      </div>
                    </div>
                    {/* Sparkline */}
                    {sparklineData.length >= 2 && (
                      <div className="mt-2 flex justify-center opacity-60">
                        <svg width="120" height="30" viewBox="0 0 120 30" className="overflow-visible">
                          <path d={sparklinePath()} fill="none" stroke="#10b981" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                        </svg>
                      </div>
                    )}
                  </>
                ) : (
                  <div className="h-14 flex items-center justify-center">
                    <span className="text-sm text-zinc-600 font-medium">
                      {t('connectToMonitor')}
                    </span>
                  </div>
                )}
              </div>

              {/* Routing + speed test */}
              <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-5 shadow-lg">
                <p className="text-zinc-500 text-[10px] font-bold uppercase tracking-widest mb-3">
                  {t('smartRouting')}
                </p>
                <div className="flex flex-col gap-3">
                  <div className="flex items-center justify-between">
                    <div className="flex items-center gap-3">
                      <div className={`w-2.5 h-2.5 rounded-full ${isConnected ? 'bg-emerald-500 shadow-[0_0_8px_rgba(16,185,129,0.6)]' : 'bg-zinc-700'}`} />
                      <span className="text-sm text-zinc-300 font-medium">
                        {isConnected ? t('bypassingLocal') : t('routingDisabled')}
                      </span>
                    </div>
                    {/* Kill switch indicator */}
                    {isConnected && settings.killSwitch && (
                      <span className="text-[10px] text-emerald-500/80 font-bold uppercase tracking-wider">
                        {t('enabled')}
                      </span>
                    )}
                  </div>
                  {/* Speed test button */}
                  {isConnected && (
                    <button
                      onClick={() => void handleSpeedTest()}
                      disabled={isSpeedTesting}
                      className="flex items-center justify-center gap-2 bg-emerald-500/10 hover:bg-emerald-500/20 text-emerald-400 rounded-xl py-2 transition-all text-xs font-semibold disabled:opacity-50"
                    >
                      <Gauge size={14} className={isSpeedTesting ? 'animate-spin' : ''} />
                      {isSpeedTesting ? t('speedTesting') : speedTestResult?.delay ? `${t('speedTestResult')}: ${String(speedTestResult.delay)} ms` : t('speedTest')}
                    </button>
                  )}
                </div>
              </div>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
