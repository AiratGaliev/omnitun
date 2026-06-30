import { useEffect, useRef, useState } from 'react';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { useAppStore } from '@/store.ts';
import {
  AlertCircle,
  AlertTriangle,
  CheckCircle2,
  Info,
  List,
  Plug,
  Terminal,
  Trash2,
} from 'lucide-react';
import { AnimatePresence, motion } from 'framer-motion';
import type { LogSource } from '@/types.ts';

const SOURCE_OPTIONS: { value: 'all' | LogSource; labelKey: string }[] = [
  { value: 'all', labelKey: 'logAllSources' },
  { value: 'system', labelKey: 'logSourceSystem' },
  { value: 'proxy', labelKey: 'logSourceProxy' },
  { value: 'installer', labelKey: 'logSourceInstaller' },
  { value: 'process', labelKey: 'logSourceProcess' },
  { value: 'subscription', labelKey: 'logSourceSubscription' },
  { value: 'settings', labelKey: 'logSourceSettings' },
];

export function Logs() {
  const { t } = useTranslation();
  const { logs, clearLogs, connections, refreshConnections, status } = useAppStore();
  const [activeFilter, setActiveFilter] = useState<'all' | LogSource>('all');
  const [tab, setTab] = useState<'logs' | 'connections'>('logs');

  const filteredLogs =
    activeFilter === 'all' ? logs : logs.filter((l) => l.source === activeFilter);

  // Poll connections every 2 seconds when connected
  const connInterval = useRef<ReturnType<typeof setInterval> | undefined>(undefined);
  useEffect(() => {
    if (tab === 'connections' && status === 'connected') {
      void refreshConnections();
      connInterval.current = setInterval(() => {
        void refreshConnections();
      }, 2000);
    } else {
      if (connInterval.current) clearInterval(connInterval.current);
    }
    return () => {
      if (connInterval.current) clearInterval(connInterval.current);
    };
  }, [tab, status, refreshConnections]);

  const getLogIcon = (level: string) => {
    switch (level) {
      case 'error':
        return <AlertCircle size={14} className="text-red-500 shrink-0" />;
      case 'warning':
        return <AlertTriangle size={14} className="text-amber-500 shrink-0" />;
      case 'success':
        return <CheckCircle2 size={14} className="text-emerald-500 shrink-0" />;
      default:
        return <Info size={14} className="text-blue-400 shrink-0" />;
    }
  };

  const getLogColor = (level: string) => {
    switch (level) {
      case 'error':
        return 'text-red-400 font-medium';
      case 'warning':
        return 'text-amber-400 font-medium';
      case 'success':
        return 'text-emerald-400 font-medium';
      default:
        return 'text-zinc-300';
    }
  };

  return (
    <div className="flex flex-col h-full w-full p-10 pt-8">
      <header className="mb-6 flex justify-between items-start gap-4">
        <div>
          <h1 className="text-3xl font-bold tracking-tight text-white">{t('logsTitle')}</h1>
          <p className="text-zinc-400 text-sm mt-1.5">{t('logsDesc')}</p>
        </div>
        {tab === 'logs' && (
          <button
            onClick={clearLogs}
            disabled={logs.length === 0}
            className="shrink-0 flex items-center justify-center gap-2 bg-red-500/10 hover:bg-red-500/20 text-red-400 px-4 py-2.5 rounded-xl text-sm font-semibold transition-all border border-red-500/20 disabled:opacity-50 shadow-sm"
          >
            <Trash2 size={18} />
            <span className="hidden sm:inline">{t('clearLogs')}</span>
          </button>
        )}
      </header>

      {/* Tab switcher */}
      <div className="flex gap-0 bg-zinc-900/60 rounded-xl p-1 mb-5 w-fit border border-white/5">
        <button
          onClick={() => {
            setTab('logs');
          }}
          className={`flex items-center gap-2 px-4 py-2 rounded-lg text-sm font-semibold transition-all ${tab === 'logs' ? 'bg-zinc-800 text-white shadow-sm' : 'text-zinc-400 hover:text-zinc-200'}`}
        >
          <Terminal size={16} />
          {t('navLogs')}
        </button>
        <button
          onClick={() => {
            setTab('connections');
          }}
          className={`flex items-center gap-2 px-4 py-2 rounded-lg text-sm font-semibold transition-all ${tab === 'connections' ? 'bg-zinc-800 text-white shadow-sm' : 'text-zinc-400 hover:text-zinc-200'}`}
        >
          <Plug size={16} />
          {t('navConnections')}
          {connections.length > 0 && (
            <span className="text-[10px] bg-emerald-500/20 text-emerald-400 px-1.5 py-0.5 rounded-full font-mono">
              {connections.length}
            </span>
          )}
        </button>
      </div>

      <div className="flex-1 overflow-hidden bg-zinc-950/80 border border-white/10 rounded-3xl shadow-2xl relative flex flex-col backdrop-blur-xl">
        {tab === 'logs' ? (
          <>
            <div className="bg-zinc-900/80 border-b border-white/5 px-5 py-3 flex items-center gap-3 text-xs font-mono text-zinc-400 shrink-0">
              <div className="flex gap-1.5 mr-4">
                <div className="w-3 h-3 rounded-full bg-red-500/80" />
                <div className="w-3 h-3 rounded-full bg-yellow-500/80" />
                <div className="w-3 h-3 rounded-full bg-emerald-500/80" />
              </div>
              <span className="font-semibold tracking-wider">omnitun-core.log</span>
              <select
                value={activeFilter}
                onChange={(e) => {
                  setActiveFilter(e.target.value as 'all' | LogSource);
                }}
                className="bg-zinc-950/50 border border-white/10 rounded-lg px-3 py-1.5 text-xs font-sans font-medium text-zinc-300 focus:outline-none focus:border-emerald-500/50 cursor-pointer transition-colors ml-auto"
              >
                {SOURCE_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value} className="bg-zinc-900 text-zinc-300">
                    {t(opt.labelKey as unknown as Parameters<typeof t>[0])}
                  </option>
                ))}
              </select>
            </div>
            <div className="flex-1 overflow-y-auto custom-scrollbar p-5 font-mono text-[13px] leading-relaxed space-y-2">
              <AnimatePresence initial={false}>
                {filteredLogs.length > 0 ? (
                  filteredLogs.map((log) => (
                    <motion.div
                      key={log.id}
                      initial={{ opacity: 0, x: -10 }}
                      animate={{ opacity: 1, x: 0 }}
                      exit={{ opacity: 0, x: 10 }}
                      className="flex gap-4 hover:bg-white/5 p-1.5 rounded-lg transition-colors group"
                    >
                      <span className="text-zinc-600 shrink-0 select-none">
                        {new Date(log.timestamp).toLocaleTimeString('en-GB', {
                          hour: '2-digit',
                          minute: '2-digit',
                          second: '2-digit',
                        })}
                        <span className="text-zinc-700">
                          .{String(new Date(log.timestamp).getMilliseconds()).padStart(3, '0')}
                        </span>
                      </span>
                      <div className="mt-0.5">{getLogIcon(log.level)}</div>
                      <span className="break-all">
                        <span className="text-zinc-500 mr-2 select-none">[{log.source}]</span>
                        <span className={getLogColor(log.level)}>{log.message}</span>
                      </span>
                    </motion.div>
                  ))
                ) : (
                  <div className="h-full flex flex-col items-center justify-center text-zinc-600 gap-4">
                    <Terminal size={32} className="opacity-30" />
                    <p className="font-sans text-sm">{t('noLogs')}</p>
                  </div>
                )}
              </AnimatePresence>
            </div>
          </>
        ) : (
          <div className="flex-1 overflow-y-auto custom-scrollbar p-5">
            {status !== 'connected' ? (
              <div className="h-full flex flex-col items-center justify-center text-zinc-600 gap-4">
                <Plug size={32} className="opacity-30" />
                <p className="font-sans text-sm">{t('connectToMonitor')}</p>
              </div>
            ) : connections.length === 0 ? (
              <div className="h-full flex flex-col items-center justify-center text-zinc-600 gap-4">
                <List size={32} className="opacity-30" />
                <p className="font-sans text-sm">{t('noConnections')}</p>
              </div>
            ) : (
              <table className="w-full text-sm font-mono">
                <thead>
                  <tr className="text-zinc-500 text-[10px] uppercase tracking-wider border-b border-white/5">
                    <th className="text-left py-2 pr-4">{t('connectionHost')}</th>
                    <th className="text-left py-2 pr-4">{t('connectionNetwork')}</th>
                    <th className="text-left py-2 pr-4">{t('connectionType')}</th>
                    <th className="text-left py-2 pr-4">{t('connectionRule')}</th>
                    <th className="text-left py-2 pr-4">{t('connectionSource')}</th>
                  </tr>
                </thead>
                <tbody>
                  {connections.map((conn) => (
                    <tr
                      key={conn.id}
                      className="border-b border-white/5 hover:bg-white/5 transition-colors"
                    >
                      <td className="py-2 pr-4 text-zinc-200 max-w-[200px] truncate">
                        {conn.metadata?.host ?? conn.metadata?.destination_ip ?? '-'}
                      </td>
                      <td className="py-2 pr-4 text-zinc-400">{conn.metadata?.network ?? '-'}</td>
                      <td className="py-2 pr-4">
                        <span className="text-[10px] bg-zinc-800 text-zinc-300 px-2 py-0.5 rounded-md">
                          {conn.metadata?.type ?? '-'}
                        </span>
                      </td>
                      <td className="py-2 pr-4 text-emerald-400 max-w-[150px] truncate">
                        {conn.rule ?? '-'}
                      </td>
                      <td className="py-2 text-zinc-500 max-w-[120px] truncate">
                        {conn.metadata?.source_ip ?? '-'}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
            <p className="text-[10px] text-zinc-600 font-mono mt-3 text-center">
              {connections.length} {t('connectionCount')}
            </p>
          </div>
        )}
      </div>
    </div>
  );
}
