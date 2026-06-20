import { motion } from 'framer-motion';
import { Languages, ShieldCheck } from 'lucide-react';
import { toast } from 'sonner';
import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { i18n } from '../../i18n';
import { AddProfileOptions } from '../AddProfileOptions';

export function Onboarding() {
  const addLog = useVPNStore((s) => s.addLog);
  const completeOnboarding = useVPNStore((state) => state.completeOnboarding);
  const addProfile = useVPNStore((state) => state.addProfile);
  const { updateSettings } = useVPNStore();
  const { t, lang } = useTranslation();

  const handleConnectImpl = async (linkStr: string, customName?: string) => {
    try {
      addLog('Adding initial subscription...', 'info', 'system');
      if (linkStr) await addProfile({ name: customName ?? 'Initial Profile', url: linkStr });
      toast.success(t('addedSuccess'));
      completeOnboarding();
    } catch {
      addLog('Failed to add profile during onboarding', 'error', 'system');
      toast.error(t('addFailed'));
    }
  };

  return (
    <motion.div
      initial={{ opacity: 0, scale: 0.98, filter: 'blur(4px)' }}
      animate={{ opacity: 1, scale: 1, filter: 'blur(0px)' }}
      exit={{ opacity: 0, scale: 0.95 }}
      transition={{ duration: 0.5, ease: 'easeOut' }}
      className="flex flex-col items-center justify-center w-full h-full p-8 text-center relative bg-gradient-to-br from-zinc-950 to-zinc-900"
    >
      <div className="absolute top-8 right-8 flex items-center gap-3 bg-zinc-900/50 p-1.5 pr-3 rounded-2xl border border-white/5 backdrop-blur-md">
        <div className="p-2 bg-zinc-800/80 rounded-xl">
          <Languages size={16} className="text-zinc-400" />
        </div>
        <div className="relative">
          <select
            value={lang}
            onChange={(e) => {
              const language = e.target.value as 'en' | 'ru';
              void i18n.changeLanguage(language);
              updateSettings({ language });
              addLog(`Language changed to ${language} (onboarding)`, 'info', 'settings');
              void useVPNStore.getState().saveSettings();
            }}
            className="appearance-none bg-transparent text-sm font-medium focus:outline-none cursor-pointer text-zinc-200 pr-4"
          >
            <option value="en" className="bg-zinc-900 text-white">English</option>
            <option value="ru" className="bg-zinc-900 text-white">Русский</option>
          </select>
        </div>
      </div>

      <div className="relative mb-10">
        <div className="absolute inset-0 scale-[1.8] rounded-full border border-emerald-500/20 bg-emerald-500/5 pointer-events-none animate-pulse" />
        <div className="w-24 h-24 bg-zinc-900/80 border border-white/10 text-emerald-400 rounded-[2rem] flex items-center justify-center shadow-2xl relative z-10 backdrop-blur-md">
          <ShieldCheck size={48} strokeWidth={1.5} />
        </div>
      </div>

      <h1 className="text-3xl font-bold tracking-tight mb-3 text-white">{t('secureTunnelSetup')}</h1>
      <p className="text-zinc-400 mb-12 text-sm max-w-md leading-relaxed">{t('secureTunnelDesc')}</p>

      <AddProfileOptions onAdd={(url, name) => void handleConnectImpl(url, name)} horizontal={true} />

      <button
        onClick={() => {
          addLog('Onboarding skipped', 'info', 'system');
          completeOnboarding();
        }}
        className="mt-8 px-6 py-2.5 rounded-xl border border-transparent text-zinc-500 hover:text-zinc-300 hover:bg-white/5 transition-all text-sm font-medium"
      >
        {t('skip') || 'Skip'}
      </button>
    </motion.div>
  );
}
