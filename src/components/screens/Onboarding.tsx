import { motion } from 'framer-motion';
import { ShieldCheck, Languages } from 'lucide-react';
import { toast } from 'sonner';
import { useVPNStore } from '../../store';
import { useTranslation } from '../../hooks/useTranslation';
import { i18n } from '../../i18n';
import { AddProfileOptions } from '../AddProfileOptions';

export function Onboarding() {
  const completeOnboarding = useVPNStore((state) => state.completeOnboarding);
  const addProfile = useVPNStore((state) => state.addProfile);
  const { t, lang } = useTranslation();

  const handleConnectImpl = async (linkStr: string, customName?: string) => {
    try {
      if (linkStr) {
        await addProfile({
          name: customName ?? 'Initial Profile',
          url: linkStr,
        });
      }
      toast.success(t('addedSuccess'));
      completeOnboarding();
    } catch {
      toast.error(t('addFailed'));
    }
  };

  return (
    <motion.div
      initial={{ opacity: 0, scale: 0.98 }}
      animate={{ opacity: 1, scale: 1 }}
      exit={{ opacity: 0, scale: 0.95 }}
      className="flex flex-col items-center justify-center w-full h-full p-8 text-center relative"
    >
      <div className="absolute top-6 right-6 flex items-center gap-2">
        <Languages size={16} className="text-zinc-500" />
        <div className="relative z-50">
          <select
            value={lang}
            onChange={(e) => {
              void i18n.changeLanguage(e.target.value);
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

      <div className="relative">
        <div className="absolute inset-0 scale-[1.5] rounded-full border border-emerald-500/20 bg-emerald-500/5 pointer-events-none" />
        <div className="w-24 h-24 bg-zinc-900 border border-white/10 text-emerald-500 rounded-3xl flex items-center justify-center mb-10 shadow-2xl relative z-10">
          <ShieldCheck size={48} strokeWidth={1.5} />
        </div>
      </div>

      <h1 className="text-3xl font-bold tracking-tight mb-3">{t('secureTunnelSetup')}</h1>
      <p className="text-zinc-500 mb-10 text-sm max-w-md">{t('secureTunnelDesc')}</p>

      <AddProfileOptions
        onAdd={(url, name) => {
          void handleConnectImpl(url, name);
        }}
        horizontal={true}
      />

      <button
        onClick={() => {
          completeOnboarding();
        }}
        className="mt-6 px-6 py-2 rounded-full border border-white/10 text-zinc-400 hover:text-white hover:bg-white/5 transition-colors text-sm"
      >
        {t('skip') || 'Skip'}
      </button>
    </motion.div>
  );
}
