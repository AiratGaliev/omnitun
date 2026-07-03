import { useState } from 'react';
import { useAppStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { i18n } from '../../i18n';
import {
  ChevronDown,
  ExternalLink,
  Key,
  Languages,
  Minus,
  RefreshCw,
  Rocket,
  Zap,
} from 'lucide-react';
import { openUrl } from '@tauri-apps/plugin-opener';
import { SettingItem } from './SettingItem';

export function GeneralSettings() {
  const { settings, updateSettings, addLog } = useAppStore();
  const { t, lang } = useTranslation();
  const [showIpinfoHelp, setShowIpinfoHelp] = useState(false);

  return (
    <div className="space-y-4">
      <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 hover:bg-zinc-900/60 hover:border-white/10 transition-all duration-300 rounded-2xl p-6 flex items-center justify-between gap-6 shadow-sm group">
        <div className="flex-1 flex gap-5">
          <div className="mt-0.5 bg-zinc-800/80 p-2.5 rounded-xl shadow-inner h-fit border border-white/5 group-hover:border-white/10 transition-colors">
            <Languages size={20} className="text-purple-400" />
          </div>
          <div>
            <h3 className="font-semibold text-sm text-zinc-100 tracking-wide">{t('language')}</h3>
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
            className="appearance-none bg-zinc-800/80 border border-white/10 rounded-xl pl-4 pr-10 py-2.5 text-sm font-medium focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none cursor-pointer transition-all shadow-inner"
          >
            <option value="en" className="bg-zinc-900 text-white">
              English (US)
            </option>
            <option value="ru" className="bg-zinc-900 text-white">
              Русский (RU)
            </option>
          </select>
          <div className="absolute right-3 top-1/2 -translate-y-1/2 pointer-events-none text-zinc-400">
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2.5"
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

      <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 hover:bg-zinc-900/60 hover:border-white/10 transition-all duration-300 rounded-2xl p-6 flex items-center justify-between gap-6 shadow-sm group">
        <div className="flex-1 flex gap-5">
          <div className="mt-0.5 bg-zinc-800/80 p-2.5 rounded-xl shadow-inner h-fit border border-white/5 group-hover:border-white/10 transition-colors">
            <RefreshCw size={20} className="text-cyan-400" />
          </div>
          <div>
            <h3 className="font-semibold text-sm text-zinc-100 tracking-wide">
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
            className="appearance-none bg-zinc-800/80 border border-white/10 rounded-xl pl-4 pr-10 py-2.5 text-sm font-medium focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none cursor-pointer transition-all shadow-inner"
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
          <div className="absolute right-3 top-1/2 -translate-y-1/2 pointer-events-none text-zinc-400">
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2.5"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="m6 9 6 6 6-6" />
            </svg>
          </div>
        </div>
      </div>

      <SettingItem
        icon={<Minus size={20} className="text-indigo-400" />}
        title={t('minimizeToTray')}
        description={t('minimizeToTrayDesc')}
        checked={settings.minimizeToTray}
        onChange={(checked) => {
          updateSettings({ minimizeToTray: checked });
        }}
      />

      {/* IPinfo.io API token */}
      <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 hover:bg-zinc-900/60 hover:border-white/10 transition-all duration-300 rounded-2xl p-6 shadow-sm group">
        <div className="flex gap-5">
          <div className="mt-0.5 bg-zinc-800/80 p-2.5 rounded-xl shadow-inner h-fit border border-white/5 group-hover:border-white/10 transition-colors">
            <Key size={20} className="text-amber-400" />
          </div>
          <div className="flex-1 min-w-0">
            <h3 className="font-semibold text-sm text-zinc-100 tracking-wide">
              {t('ipinfoToken')}
            </h3>
            <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">{t('ipinfoTokenDesc')}</p>
            <input
              type="text"
              value={settings.ipinfoToken}
              onChange={(e) => {
                updateSettings({ ipinfoToken: e.target.value });
              }}
              placeholder={t('ipinfoTokenPlaceholder')}
              className="mt-3 w-full bg-zinc-800/60 border border-white/10 rounded-xl px-4 py-2.5 text-sm font-mono text-zinc-200 placeholder-zinc-500 focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none transition-all shadow-inner"
            />

            {/* Get token link */}
            <button
              onClick={() => {
                void openUrl('https://ipinfo.io/signup');
                addLog('Opened ipinfo.io signup page', 'info', 'settings');
              }}
              className="mt-2.5 inline-flex items-center gap-1.5 text-xs text-amber-400/70 hover:text-amber-300 transition-colors group/link"
            >
              <ExternalLink size={14} className="shrink-0" />
              <span>{t('ipinfoGetToken') || 'Get a free API token at ipinfo.io'}</span>
              <span className="inline-block transition-transform duration-200 group-hover/link:translate-x-0.5">
                →
              </span>
            </button>

            {/* How to get your token — toggle */}
            <button
              onClick={() => {
                setShowIpinfoHelp(!showIpinfoHelp);
              }}
              className="mt-2 inline-flex items-center gap-1.5 text-xs text-zinc-500 hover:text-zinc-300 transition-colors"
            >
              <ChevronDown
                size={14}
                className={`transition-transform duration-200 ${showIpinfoHelp ? 'rotate-180' : ''}`}
              />
              <span>{t('ipinfoHowToGet') || 'How to get your API token'}</span>
            </button>

            {/* Collapsible instructions */}
            {showIpinfoHelp && (
              <div className="mt-3 bg-zinc-800/30 border border-white/5 rounded-xl p-4 space-y-2.5 text-xs text-zinc-400 leading-relaxed">
                <div className="flex items-start gap-2.5">
                  <span className="flex items-center justify-center w-5 h-5 rounded-full bg-amber-500/10 text-amber-400 text-[10px] font-bold shrink-0 mt-0.5">
                    1
                  </span>
                  <span>
                    {t('ipinfoStep1') || 'Go to'}{' '}
                    <button
                      onClick={() => void openUrl('https://ipinfo.io/signup')}
                      className="text-amber-400/70 hover:text-amber-300 underline underline-offset-2 transition-colors"
                    >
                      ipinfo.io/signup
                    </button>{' '}
                    {t('ipinfoStep1End') || 'and create a free account'}
                  </span>
                </div>
                <div className="flex items-start gap-2.5">
                  <span className="flex items-center justify-center w-5 h-5 rounded-full bg-amber-500/10 text-amber-400 text-[10px] font-bold shrink-0 mt-0.5">
                    2
                  </span>
                  <span>{t('ipinfoStep2') || 'Log in and navigate to your API dashboard'}</span>
                </div>
                <div className="flex items-start gap-2.5">
                  <span className="flex items-center justify-center w-5 h-5 rounded-full bg-amber-500/10 text-amber-400 text-[10px] font-bold shrink-0 mt-0.5">
                    3
                  </span>
                  <span>
                    {t('ipinfoStep3') || 'Copy your personal API token from the dashboard'}
                  </span>
                </div>
                <div className="flex items-start gap-2.5">
                  <span className="flex items-center justify-center w-5 h-5 rounded-full bg-amber-500/10 text-amber-400 text-[10px] font-bold shrink-0 mt-0.5">
                    4
                  </span>
                  <span>
                    {t('ipinfoStep4') ||
                      'Paste it above — the app will download the GeoIP database automatically'}
                  </span>
                </div>
                <div className="mt-2 pt-2.5 border-t border-white/5 text-zinc-500">
                  <span>
                    💡{' '}
                    {t('ipinfoWhy') ||
                      'A free IPinfo account gives you 1,000 lookups/day — enough for personal use. Your token stays on-device and is never shared.'}
                  </span>
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
