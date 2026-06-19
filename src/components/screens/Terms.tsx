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
      initial={{ opacity: 0, scale: 0.98 }}
      animate={{ opacity: 1, scale: 1 }}
      exit={{ opacity: 0, scale: 0.95 }}
      className="flex flex-col items-center justify-center w-full h-full p-8 text-center relative"
    >
      <div className="absolute top-6 right-6 flex items-center gap-2">
        <Languages size={16} className="text-zinc-500" />
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
            className="appearance-none bg-black/20 border border-white/10 rounded-lg pl-3 pr-8 py-1.5 text-sm focus:ring-2 focus:ring-emerald-500/20 focus:border-emerald-500/50 outline-none cursor-pointer transition-colors"
          >
            <option value="en" className="bg-zinc-900 text-white">
              English (US)
            </option>
            <option value="ru" className="bg-zinc-900 text-white">
              Русский (RU)
            </option>
          </select>
          <div className="absolute right-2 top-1/2 -translate-y-1/2 pointer-events-none text-zinc-500">
            <svg
              width="12"
              height="12"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="m6 9 6 6 6-6" />
            </svg>
          </div>
        </div>
      </div>

      <div className="w-16 h-16 bg-white/5 border border-white/10 text-yellow-500 rounded-2xl flex items-center justify-center mb-6 shadow-lg">
        <ShieldAlert size={32} />
      </div>

      <h1 className="text-2xl font-bold tracking-tight mb-2">{t('termsTitle')}</h1>
      <p className="text-zinc-500 mb-8 text-sm max-w-sm">{t('termsDesc')}</p>

      <div className="w-full max-w-md bg-black/30 border border-white/10 rounded-2xl p-6 mb-8 text-left h-64 overflow-y-auto custom-scrollbar flex flex-col gap-4 text-sm text-zinc-300 leading-relaxed shadow-inner">
        <p>{t('termsText')}</p>
      </div>

      <div className="w-full max-w-md">
        <button
          onClick={() => {
            updateSettings({ acceptTerms: true });
            addLog('Terms of service accepted', 'success', 'system');
            void useVPNStore.getState().saveSettings();
          }}
          className="w-full bg-emerald-500 hover:bg-emerald-400 text-black font-bold rounded-xl py-4 flex items-center justify-center gap-2 transition-all active:scale-[0.98] shadow-lg shadow-emerald-500/20"
        >
          <Check size={20} />
          {t('acceptTermsBtn')}
        </button>
      </div>
    </motion.div>
  );
}
