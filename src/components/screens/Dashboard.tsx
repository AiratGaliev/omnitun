import { useEffect, useState } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import { Power, ChevronDown, Check } from 'lucide-react';
import { toast } from 'sonner';
import { useVPNStore } from '../../store';
import { useTranslation } from '../../hooks/useTranslation';
import { SegmentedControl } from '../SegmentedControl';

export function Dashboard() {
  const {
    status,
    activeServer,
    servers,
    speeds,
    setSpeeds,
    setActiveServer,
    settings,
    updateSettings,
  } = useVPNStore();
  const toggleConnection = useVPNStore((state) => state.toggleConnection);
  const { t } = useTranslation();
  const [isServerDropdownOpen, setIsServerDropdownOpen] = useState(false);

  const [sessionStart, setSessionStart] = useState<number | null>(null);
  const [sessionDuration, setSessionDuration] = useState<string>('00:00:00');

  useEffect(() => {
    if (status === 'connected') {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setSessionStart(Date.now());
    } else {
      setSessionStart(null);
      setSessionDuration('00:00:00');
    }
  }, [status]);

  useEffect(() => {
    let interval: ReturnType<typeof setInterval>;
    if (status === 'connected' && sessionStart) {
      interval = setInterval(() => {
        setSpeeds({
          down: Math.random() * 8 + 1,
          up: Math.random() * 2 + 0.1,
        });

        const diff = Math.floor((Date.now() - sessionStart) / 1000);
        const h = Math.floor(diff / 3600)
          .toString()
          .padStart(2, '0');
        const m = Math.floor((diff % 3600) / 60)
          .toString()
          .padStart(2, '0');
        const s = (diff % 60).toString().padStart(2, '0');
        setSessionDuration(`${h}:${m}:${s}`);
      }, 1000);
    } else {
      setSpeeds({ up: 0, down: 0 });
    }
    return () => {
      clearInterval(interval);
    };
  }, [status, setSpeeds, sessionStart]);

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
      const msg = typeof e === 'string' ? e : e instanceof Error ? e.message : 'Connection failed';
      toast.error(msg, {
        description: 'Check your configuration and try again',
      });
    }
  };

  const isConnected = status === 'connected';
  const isConnecting = status === 'connecting';

  return (
    <div className="flex flex-col h-full w-full p-8 flex-1">
      {/* Header */}
      <header className="flex justify-between items-center mb-6">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">OmniTun</h1>
          <p className="text-zinc-500 text-sm">{t('secureTunnelSubtitle')}</p>
        </div>
        <div className="flex gap-3 relative">
          <motion.button
            whileTap={{ scale: 0.95 }}
            onClick={() => {
              setIsServerDropdownOpen(!isServerDropdownOpen);
            }}
            className="px-4 py-2 bg-white/5 hover:bg-white/10 backdrop-blur-md transition-colors rounded-xl border border-white/10 flex items-center gap-2 relative z-20"
          >
            <span className="text-lg">{activeServer?.flag}</span>
            <span className="font-medium text-sm text-zinc-100">{activeServer?.name}</span>
            <div className="flex items-center gap-1 ml-2">
              <div
                className={`w-1.5 h-1.5 rounded-full ${isConnected ? 'bg-emerald-500 animate-pulse shadow-[0_0_8px_rgba(16,185,129,0.5)]' : 'bg-red-500'}`}
              />
              <span className="text-xs font-mono text-zinc-400 w-10 text-right">
                {activeServer?.ping} ms
              </span>
            </div>
            <ChevronDown
              size={16}
              className={`ml-1 text-zinc-500 transition-transform ${isServerDropdownOpen ? 'rotate-180' : ''}`}
            />
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
                  initial={{ opacity: 0, y: -10 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: -10 }}
                  className="absolute top-full right-0 mt-2 w-64 bg-zinc-900/95 backdrop-blur-xl border border-white/10 rounded-xl shadow-2xl p-2 z-20 max-h-64 overflow-y-auto custom-scrollbar"
                >
                  <p className="text-xs font-bold text-zinc-500 uppercase tracking-widest px-3 py-2 border-b border-white/5 mb-2">
                    {t('availableEndpoints')} ({servers.length})
                  </p>
                  <div className="flex flex-col gap-1">
                    {servers.map((server) => (
                      <button
                        key={server.id}
                        onClick={() => {
                          setActiveServer(server);
                          setIsServerDropdownOpen(false);
                          toast.success(`${t('serverStr')}${server.name}${t('selectedStr')}`);
                        }}
                        disabled={status === 'connecting'}
                        className={`flex items-center justify-between p-2 rounded-lg transition-colors ${
                          activeServer?.id === server.id
                            ? 'bg-emerald-500/10 text-emerald-400'
                            : 'hover:bg-white/5 text-zinc-300'
                        }`}
                      >
                        <div className="flex items-center gap-2">
                          <span>{server.flag}</span>
                          <span className="text-sm font-medium">{server.name}</span>
                        </div>
                        <div className="flex items-center gap-1">
                          <span
                            className={`text-[10px] font-mono px-1.5 py-0.5 rounded ${(server.ping ?? 999) < 60 ? 'bg-emerald-500/20 text-emerald-400' : 'bg-yellow-500/20 text-yellow-400'}`}
                          >
                            {server.ping ?? '-'}ms
                          </span>
                          {activeServer?.id === server.id && <Check size={14} className="ml-1" />}
                        </div>
                      </button>
                    ))}
                    {servers.length === 0 && (
                      <p className="text-xs text-zinc-500 text-center py-4">
                        {t('noServersAvailable')}
                      </p>
                    )}
                  </div>
                </motion.div>
              </>
            )}
          </AnimatePresence>
        </div>
      </header>

      <div className="flex-1 flex gap-10 items-center justify-center">
        {/* Center - Control Panels */}
        <div className="flex flex-col items-center justify-center">
          <div className="mb-6 w-80 flex flex-col items-center">
            <SegmentedControl
              options={[
                { id: 'vpn', label: t('vpnMode') },
                { id: 'proxy', label: t('proxyMode') },
              ]}
              value={settings.routingMode}
              onChange={(val) => {
                updateSettings({ routingMode: val });
              }}
              disabled={isConnected || isConnecting}
              layoutId="modeIndicatorDashboard"
            />

            <div className="h-24 mt-4 relative w-full">
              <AnimatePresence mode="wait" initial={false}>
                <motion.div
                  key={settings.routingMode}
                  initial={{ opacity: 0, y: 10 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: -10 }}
                  transition={{ duration: 0.15 }}
                  className="absolute inset-0 flex flex-col items-center justify-start text-center"
                >
                  <p className="text-xs text-zinc-500">
                    {settings.routingMode === 'vpn' ? t('vpnModeDesc') : t('proxyModeDesc')}
                  </p>
                  {settings.routingMode === 'proxy' && (
                    <motion.div className="flex items-center justify-between gap-2 mt-4 bg-black/20 w-[140px] rounded-lg border border-white/10 p-1.5 px-3 transition-all shadow-inner focus-within:ring-2 focus-within:ring-emerald-500/20 focus-within:border-emerald-500/50">
                      <span className="text-xs text-zinc-500 font-medium tracking-wide">
                        {t('proxyPort')}:
                      </span>
                      <input
                        type="number"
                        value={settings.proxyPort || 1080}
                        onChange={(e) => {
                          updateSettings({ proxyPort: Number(e.target.value) });
                        }}
                        disabled={isConnected || isConnecting}
                        className="bg-transparent text-sm tracking-widest text-emerald-400 w-16 focus:outline-none font-mono text-right disabled:opacity-50"
                      />
                    </motion.div>
                  )}
                </motion.div>
              </AnimatePresence>
            </div>
          </div>

          <div className="relative flex items-center justify-center">
            {isConnected && (
              <>
                <motion.div
                  initial={{ scale: 0.8, opacity: 0 }}
                  animate={{ scale: 1.4, opacity: 0 }}
                  transition={{ repeat: Infinity, duration: 2, ease: 'easeOut' }}
                  className="absolute inset-0 rounded-full border border-emerald-500/20 bg-emerald-500/5"
                />
                <motion.div
                  initial={{ scale: 0.8, opacity: 0 }}
                  animate={{ scale: 1.2, opacity: 0 }}
                  transition={{ repeat: Infinity, duration: 2, delay: 0.5, ease: 'easeOut' }}
                  className="absolute inset-0 rounded-full border border-emerald-500/30"
                />
              </>
            )}

            <motion.button
              whileHover={{ scale: 1.05 }}
              whileTap={{ scale: 0.95 }}
              onClick={() => {
                void handleToggleConnection();
              }}
              disabled={isConnecting}
              className={`relative z-10 w-48 h-48 rounded-full border-4 flex flex-col items-center justify-center transition-all duration-500 group shadow-2xl
                ${isConnected ? 'bg-zinc-900 border-emerald-500 shadow-[0_0_50px_rgba(16,185,129,0.2)]' : 'bg-zinc-900 border-zinc-800 shadow-[0_0_30px_rgba(0,0,0,0.5)]'}
                ${isConnecting ? 'animate-pulse border-blue-500 shadow-[0_0_30px_rgba(59,130,246,0.3)]' : ''}
                ${status === 'error' ? 'border-red-500 shadow-[0_0_30px_rgba(239,68,68,0.3)]' : ''}
              `}
            >
              <Power
                size={48}
                className={`mb-2 transition-colors duration-500 ${isConnected ? 'text-emerald-500' : isConnecting ? 'text-blue-500' : status === 'error' ? 'text-red-500' : 'text-zinc-500'}`}
              />
              <span
                className={`text-xs font-bold uppercase tracking-widest ${isConnected ? 'text-emerald-500' : isConnecting ? 'text-blue-500' : status === 'error' ? 'text-red-500' : 'text-zinc-500'}`}
              >
                {status === 'error'
                  ? 'Error'
                  : isConnected
                    ? t('connected')
                    : isConnecting
                      ? t('connecting')
                      : t('disconnected')}
              </span>
            </motion.button>
          </div>

          <div className="mt-8 text-center h-[50px]">
            {isConnected && (
              <motion.div
                initial={{ opacity: 0 }}
                animate={{ opacity: 1 }}
                className="flex flex-col items-center text-center"
              >
                <p className="text-3xl font-light font-mono tracking-tight text-zinc-100">
                  {sessionDuration}
                </p>
                <p className="text-zinc-500 text-xs uppercase tracking-widest mt-1 font-semibold">
                  {t('activeSession')}
                </p>
              </motion.div>
            )}
          </div>
        </div>

        {/* Side Stats Panel */}
        <div className="w-72 flex flex-col gap-4">
          <div className="bg-white/5 border border-white/5 rounded-2xl p-4">
            <p className="text-zinc-500 text-xs font-bold uppercase mb-2">{t('protocolStatus')}</p>
            <div className="flex items-center justify-between">
              <span className="text-sm font-medium">
                {isConnected ? 'VLESS + Reality' : t('standby')}
              </span>
              <span
                className={`text-xs font-mono ${isConnected ? 'text-emerald-500' : 'text-zinc-500'}`}
              >
                {isConnected ? t('stable') : t('idle')}
              </span>
            </div>
          </div>

          <div className="bg-white/5 border border-white/5 rounded-2xl p-4">
            <p className="text-zinc-500 text-xs font-bold uppercase mb-2">{t('networkTraffic')}</p>
            {isConnected ? (
              <div className="flex justify-between items-end gap-2">
                <div className="flex flex-col">
                  <span className="text-emerald-400 font-mono text-lg">
                    {speeds.down.toFixed(1)}
                  </span>
                  <span className="text-[10px] text-zinc-500">MB/s {t('down')}</span>
                </div>
                <div className="flex-1 h-8 flex items-end gap-0.5 justify-center opacity-80">
                  <div className="w-1 h-3 bg-zinc-700 rounded-full" />
                  <div className="w-1 h-6 bg-zinc-700 rounded-full" />
                  <div className="w-1 h-2 bg-emerald-500 rounded-full animate-pulse" />
                  <div className="w-1 h-7 bg-emerald-500 rounded-full animate-pulse" />
                  <div className="w-1 h-4 bg-emerald-500 rounded-full animate-pulse" />
                  <div className="w-1 h-5 bg-emerald-500 rounded-full animate-pulse" />
                </div>
                <div className="flex flex-col items-end">
                  <span className="text-blue-400 font-mono text-lg">{speeds.up.toFixed(1)}</span>
                  <span className="text-[10px] text-zinc-500">MB/s {t('up')}</span>
                </div>
              </div>
            ) : (
              <span className="text-sm text-zinc-600 block mt-2 text-center">
                {t('connectToMonitor')}
              </span>
            )}
          </div>

          <div className="bg-white/5 border border-white/5 rounded-2xl p-4">
            <p className="text-zinc-500 text-xs font-bold uppercase mb-2">{t('smartRouting')}</p>
            <div className="flex items-center gap-2">
              <div
                className={`w-2 h-2 rounded-full ${isConnected ? 'bg-emerald-500' : status === 'error' ? 'bg-red-500' : 'bg-red-500'}`}
              />
              <span className="text-xs text-zinc-300 italic">
                {isConnected ? t('bypassingLocal') : t('routingDisabled')}
              </span>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
