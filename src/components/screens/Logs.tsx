import { useTranslation } from '../../hooks/useTranslation';
import { useVPNStore } from '../../store';
import { Terminal, Trash2, AlertCircle, Info, CheckCircle2, AlertTriangle, Play } from 'lucide-react';
import { motion, AnimatePresence } from 'framer-motion';

export function Logs() {
  const { t } = useTranslation();
  const { logs, clearLogs } = useVPNStore();

  const getLogIcon = (type: string) => {
    switch (type) {
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

  const getLogColor = (type: string) => {
    switch (type) {
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
    <div className="flex flex-col h-full w-full p-8 pt-6">
      <header className="mb-6 flex justify-between items-start gap-4">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">{t('logsTitle')}</h1>
          <p className="text-zinc-500 text-sm mt-1">{t('logsDesc')}</p>
        </div>
        <button
          onClick={clearLogs}
          disabled={logs.length === 0}
          className="shrink-0 flex items-center justify-center gap-2 bg-red-500/10 hover:bg-red-500/20 text-red-400 px-3 py-1.5 md:px-4 md:py-2 rounded-xl text-xs md:text-sm font-medium transition-colors border border-red-500/20 disabled:opacity-50"
        >
          <Trash2 size={16} />
          <span className="hidden sm:inline">{t('clearLogs')}</span>
        </button>
      </header>

      <div className="flex-1 overflow-hidden bg-black/40 border border-white/5 rounded-xl shadow-inner relative flex flex-col">
        <div className="bg-white/5 border-b border-white/5 rounded-t-xl px-4 py-2 flex items-center gap-2 text-xs font-mono text-zinc-500 shrink-0">
          <Terminal size={14} />
          System Event Stream
        </div>

        <div className="flex-1 overflow-y-auto custom-scrollbar p-4 font-mono text-xs leading-relaxed space-y-1.5">
          <AnimatePresence initial={false}>
            {logs.length > 0 ? (
              logs.map((log) => (
                <motion.div
                  key={log.id}
                  initial={{ opacity: 0, x: -10 }}
                  animate={{ opacity: 1, x: 0 }}
                  exit={{ opacity: 0, x: 10 }}
                  className="flex gap-3 hover:bg-white/5 p-1 rounded transition-colors group"
                >
                  <span className="text-zinc-600 shrink-0">
                    {new Date(log.timestamp).toLocaleTimeString('en-GB', {
                      hour: '2-digit',
                      minute: '2-digit',
                      second: '2-digit',
                    })}.{String(new Date(log.timestamp).getMilliseconds()).padStart(3, '0')}
                  </span>
                  <div className="mt-0.5">{getLogIcon(log.type)}</div>
                  <span className={`break-all ${getLogColor(log.type)}`}>
                    {log.message}
                  </span>
                </motion.div>
              ))
            ) : (
              <div className="h-full flex flex-col items-center justify-center text-zinc-600 gap-3">
                <Play size={24} className="opacity-50" />
                <p>{t('noLogs')}</p>
              </div>
            )}
          </AnimatePresence>
        </div>
      </div>
    </div>
  );
}
