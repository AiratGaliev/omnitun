import { motion } from 'framer-motion';
import { Check, Languages, ShieldAlert } from 'lucide-react';
import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { i18n } from '../../i18n';

export function Terms() {
  const addLog = useVPNStore((s) => s.addLog);
  const { updateSettings } = useVPNStore();
  const { t, lang } = useTranslation();

  return (
    <motion.div
      initial={{ opacity: 0, scale: 0.98, filter: 'blur(4px)' }}
      animate={{ opacity: 1, scale: 1, filter: 'blur(0px)' }}
      exit={{ opacity: 0, scale: 0.95 }}
      transition={{ duration: 0.5, ease: 'easeOut' }}
      className="flex flex-col items-center justify-center w-full h-full p-8 text-center relative from-zinc-950"
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
              addLog(`Language changed to ${language} (before terms)`, 'info', 'settings');
              void useVPNStore.getState().saveSettings();
            }}
            className="appearance-none bg-transparent text-sm font-medium focus:outline-none cursor-pointer text-zinc-200 pr-4"
          >
            <option value="en" className="bg-zinc-900 text-white">
              English
            </option>
            <option value="ru" className="bg-zinc-900 text-white">
              Русский
            </option>
          </select>
        </div>
      </div>

      <div className="w-20 h-20 bg-zinc-900/80 border border-white/10 text-yellow-500 rounded-3xl flex items-center justify-center mb-8 shadow-2xl backdrop-blur-md">
        <ShieldAlert size={36} strokeWidth={1.5} />
      </div>

      <h1 className="text-3xl font-bold tracking-tight mb-3 text-white">{t('termsTitle')}</h1>
      <p className="text-zinc-400 mb-10 text-sm max-w-sm">{t('termsDesc')}</p>

      <div className="w-full max-w-lg bg-zinc-950/50 border border-white/5 rounded-3xl p-8 mb-10 text-left h-72 overflow-y-auto custom-scrollbar flex flex-col gap-4 text-sm text-zinc-300 leading-relaxed shadow-inner">
        <p>{t('termsText')}</p>
      </div>

      <div className="w-full max-w-lg">
        <button
          onClick={() => {
            updateSettings({ acceptTerms: true });
            addLog('Terms of service accepted', 'success', 'system');
            void useVPNStore.getState().saveSettings();
          }}
          className="w-full bg-emerald-500 hover:bg-emerald-400 text-zinc-950 font-bold rounded-2xl py-4 flex items-center justify-center gap-3 transition-all active:scale-[0.98] shadow-[0_0_30px_rgba(16,185,129,0.25)]"
        >
          <Check size={22} strokeWidth={2.5} />
          {t('acceptTermsBtn')}
        </button>
      </div>
    </motion.div>
  );
}
