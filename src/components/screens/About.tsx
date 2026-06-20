import { useState } from 'react';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { RefreshCw, ShieldAlert } from 'lucide-react';
import { toast } from 'sonner';
import { check } from '@tauri-apps/plugin-updater';
import { useVPNStore } from '@/store.ts';

export function About() {
  const { t } = useTranslation();
  const addLog = useVPNStore((s) => s.addLog);
  const [checkingForUpdates, setCheckingForUpdates] = useState(false);

  const handleUpdateCheck = async () => {
    try {
      setCheckingForUpdates(true);
      addLog('Checking for updates...', 'info', 'system');
      const update = await check();
      if (update) {
        addLog(`Update v${update.version} available, installing...`, 'info', 'system');
        toast(`v${update.version} ${t('updateAvailable')}`);
        await update.downloadAndInstall();
        addLog(`Update v${update.version} installed`, 'success', 'system');
        toast.success(t('updateInstalled'));
      } else {
        addLog('App is up-to-date', 'success', 'system');
        toast.success(t('upToDate'));
      }
    } catch {
      addLog('Update check failed', 'error', 'system');
      toast.error(t('updateCheckFailed'));
    } finally {
      setCheckingForUpdates(false);
    }
  };

  return (
    <div className="flex flex-col h-full w-full p-10 pt-8">
      <header className="mb-8">
        <h1 className="text-3xl font-bold tracking-tight text-white">{t('aboutTitle')}</h1>
        <p className="text-zinc-400 text-sm mt-1.5">{t('aboutDesc')}</p>
      </header>

      <div className="flex-1 overflow-y-auto overflow-x-hidden custom-scrollbar pb-8 max-w-3xl space-y-6">
        <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 hover:bg-zinc-900/60 transition-all duration-300 rounded-3xl p-6 flex items-center justify-between gap-6 shadow-lg">
          <div className="flex-1 flex gap-5">
            <div className="mt-0.5 bg-zinc-800/80 p-2.5 rounded-xl shadow-inner h-fit border border-white/5">
              <RefreshCw size={20} className="text-emerald-400" />
            </div>
            <div>
              <h3 className="font-semibold text-sm text-zinc-100 tracking-wide">{t('checkForUpdates')}</h3>
              <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">{t('checkForUpdatesDesc')}</p>
            </div>
          </div>
          <button
            onClick={() => void handleUpdateCheck()}
            disabled={checkingForUpdates}
            className="bg-zinc-800/80 hover:bg-zinc-700 border border-white/10 px-5 py-2.5 rounded-xl text-sm font-medium transition-all text-zinc-200 disabled:opacity-50 shadow-sm"
          >
            {checkingForUpdates ? <RefreshCw size={18} className="animate-spin text-zinc-400" /> : t('checkForUpdates')}
          </button>
        </div>

        <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-8 shadow-lg flex flex-col gap-6">
          <div className="grid grid-cols-2 gap-4 text-sm">
            <div className="bg-zinc-950/50 p-5 flex flex-col gap-2 rounded-2xl border border-white/5 shadow-inner">
              <span className="text-zinc-500 font-bold uppercase tracking-wider text-xs">{t('version')}</span>
              <span className="text-emerald-400 font-mono font-medium">v0.1.0 (Alpha)</span>
            </div>
            <a
              href="https://github.com/AiratGaliev/omnitun"
              target="_blank"
              rel="noreferrer"
              className="bg-zinc-950/50 hover:bg-zinc-800/80 transition-colors p-5 flex flex-col gap-2 rounded-2xl border border-white/5 shadow-inner group"
            >
              <span className="text-zinc-500 font-bold uppercase tracking-wider text-xs">{t('sourceCode')}</span>
              <span className="text-zinc-300 gap-2 flex items-center justify-between mt-0.5 group-hover:text-emerald-400 transition-colors">
                <span className="font-medium">GitHub Repository</span>
                <svg viewBox="0 0 24 24" fill="currentColor" className="w-4 h-4"><path d="M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z" /></svg>
              </span>
            </a>
          </div>

          <div className="bg-red-500/5 border border-red-500/10 rounded-2xl p-6 mt-2">
            <h4 className="text-red-400 font-bold uppercase tracking-wider text-xs mb-3 flex items-center gap-2">
              <ShieldAlert size={16} /> {t('disclaimer')}
            </h4>
            <p className="text-sm text-zinc-400 leading-relaxed max-w-prose">
              {t('disclaimerText')}
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}
