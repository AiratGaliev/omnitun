import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { i18n } from '../../i18n';
import { Languages, RefreshCw, Rocket, Zap } from 'lucide-react';
import { SettingItem } from './SettingItem';

export function GeneralSettings() {
  const { settings, updateSettings, addLog } = useVPNStore();
  const { t, lang } = useTranslation();

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-6 bg-white/5 border border-white/5 rounded-2xl p-5 shadow-sm">
        <div className="flex-1 flex gap-4">
          <div className="mt-1 bg-black/20 p-2 rounded-xl shadow-inner h-fit">
            <Languages size={20} className="text-purple-400" />
          </div>
          <div>
            <h3 className="font-medium text-sm text-zinc-200 tracking-wide">{t('language')}</h3>
            <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">{t('languageDesc')}</p>
          </div>
        </div>
        <div className="relative">
          <select
            value={lang}
            onChange={(e) => {
              const language = e.target.value as 'en' | 'ru';
              void i18n.changeLanguage(language);
              updateSettings({ language });
              addLog(`Language changed to ${language}`, 'info', 'settings');
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

      <SettingItem
        icon={<Rocket size={20} className="text-blue-400" />}
        title={t('launchOnBoot')}
        description={t('launchOnBootDesc')}
        checked={settings.startWithWindows}
        onChange={(checked) => {
          updateSettings({ startWithWindows: checked });
        }}
      />
      <SettingItem
        icon={<Zap size={20} className="text-yellow-400" />}
        title={t('smartNode')}
        description={t('smartNodeDesc')}
        checked={settings.autoConnectFastest}
        onChange={(checked) => {
          updateSettings({ autoConnectFastest: checked });
        }}
      />

      <div className="bg-white/5 border border-white/5 rounded-2xl p-5 shadow-sm flex items-center justify-between gap-6">
        <div className="flex-1 flex gap-4">
          <div className="mt-1 bg-black/20 p-2 rounded-xl shadow-inner h-fit">
            <RefreshCw size={20} className="text-cyan-400" />
          </div>
          <div>
            <h3 className="font-medium text-sm text-zinc-200 tracking-wide">
              {t('autoUpdateInterval')}
            </h3>
            <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">
              {t('autoUpdateIntervalDesc')}
            </p>
          </div>
        </div>
        <div className="relative">
          <select
            value={settings.autoUpdateInterval}
            onChange={(e) => {
              const val = Number(e.target.value);
              updateSettings({ autoUpdateInterval: val });
              addLog(`Auto-refresh interval set to ${String(val)} min`, 'info', 'settings');
            }}
            className="appearance-none bg-black/20 border border-white/10 rounded-lg pl-3 pr-8 py-1.5 text-sm focus:ring-2 focus:ring-emerald-500/20 focus:border-emerald-500/50 outline-none cursor-pointer transition-colors"
          >
            <option value={0} className="bg-zinc-900 text-white">
              {t('disabled')}
            </option>
            <option value={5} className="bg-zinc-900 text-white">
              5 {t('minutes')}
            </option>
            <option value={15} className="bg-zinc-900 text-white">
              15 {t('minutes')}
            </option>
            <option value={60} className="bg-zinc-900 text-white">
              1 {t('hour')}
            </option>
            <option value={720} className="bg-zinc-900 text-white">
              12 {t('hours')}
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
    </div>
  );
}
