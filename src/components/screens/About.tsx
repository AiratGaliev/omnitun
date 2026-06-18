import { useState } from 'react';
import { useTranslation } from '../../hooks/useTranslation';
import { ShieldAlert, RefreshCw, Github } from 'lucide-react';
import { toast } from 'sonner';

export function About() {
  const { t } = useTranslation();
  const [checkingForUpdates, setChecking] = useState(false);

  const handleUpdateCheck = () => {
    setChecking(true);
    toast(t('checkingForUpdates'));
    setTimeout(() => {
      setChecking(false);
      toast.success(t('upToDate'));
    }, 2000);
  };

  return (
    <div className="flex flex-col h-full w-full p-8">
      <header className="mb-8">
        <h1 className="text-2xl font-bold tracking-tight">{t('aboutTitle')}</h1>
        <p className="text-zinc-500 text-sm mt-1">{t('aboutDesc')}</p>
      </header>

      <div className="flex-1 overflow-y-auto overflow-x-hidden custom-scrollbar pb-4 space-y-6">

        <div className="bg-white/5 border border-white/5 hover:bg-white/10 transition-colors rounded-2xl p-5 flex items-center justify-between gap-6 shadow-sm">
          <div className="flex-1 flex gap-4">
            <div className="mt-1 bg-black/20 p-2 rounded-xl shadow-inner h-fit"><RefreshCw size={20} className="text-zinc-400" /></div>
            <div>
              <h3 className="font-medium text-sm text-zinc-200 tracking-wide">{t('checkForUpdates')}</h3>
              <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">{t('checkForUpdatesDesc')}</p>
            </div>
          </div>
          <button
            onClick={handleUpdateCheck}
            disabled={checkingForUpdates}
            className="bg-black/30 hover:bg-black/50 border border-white/5 px-4 py-2 rounded-lg text-sm transition-colors text-zinc-300 disabled:opacity-50"
          >
            {checkingForUpdates ? <RefreshCw size={18} className="animate-spin text-zinc-500" /> : t('checkForUpdates')}
          </button>
        </div>

        <div className="bg-white/5 border border-white/5 rounded-2xl p-6 shadow-sm flex flex-col gap-4">
          <div className="grid grid-cols-2 gap-3 mt-2 text-xs">
             <div className="bg-black/20 p-4 flex flex-col gap-2 rounded-xl border border-white/5">
               <span className="text-zinc-600 font-bold uppercase">{t('version')}</span>
               <span className="text-emerald-400 font-mono text-sm">v0.1.0 (Alpha)</span>
             </div>
             <a href="https://github.com/AiratGaliev/omnitun" target="_blank" rel="noreferrer" className="bg-black/20 hover:bg-black/40 transition-colors p-4 flex flex-col gap-2 rounded-xl border border-white/5 group">
               <span className="text-zinc-600 font-bold uppercase">{t('sourceCode')}</span>
               <span className="text-zinc-300 gap-2 flex items-center justify-between mt-0.5 group-hover:text-emerald-400 transition-colors">
                 <span className="font-medium text-sm">GitHub Repository</span>
                 <Github size={16} />
               </span>
             </a>
          </div>

          <div className="bg-black/30 border border-white/5 rounded-xl p-5 mt-2">
             <h4 className="text-red-400 font-bold uppercase tracking-wider text-xs mb-3 flex items-center gap-2">
               <ShieldAlert size={16} /> {t('disclaimer')}
             </h4>
             <p className="text-xs text-zinc-400 leading-relaxed max-w-prose">
               {t('disclaimerText')}
             </p>
          </div>

        </div>
      </div>
    </div>
  );
}
