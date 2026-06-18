import { motion, AnimatePresence } from 'framer-motion';
import { useVPNStore } from '../../store';
import {
  ShieldAlert,
  Zap,
  Rocket,
  Network,
  Server,
  Globe,
  PowerOff,
  Languages,
  Plus,
  RefreshCw,
  Lock,
  Check,
  Loader2,
  Timer,
} from 'lucide-react';
import { toast } from 'sonner';
import { useState, useEffect, useRef, type ReactNode } from 'react';
import { useTranslation } from '../../hooks/useTranslation';
import { i18n } from '../../i18n';
import * as z from 'zod';
import { SegmentedControl } from '../SegmentedControl';

const countrySchema = z.object({
  code: z.string().min(2).max(3),
  name: z.string().min(2),
  flag: z.string().min(1),
});

export function Settings() {
  const [activeTab, setActiveTab] = useState<'general' | 'network' | 'filtering' | 'protocols'>(
    'general',
  );
  const { t } = useTranslation();
  const { settings } = useVPNStore();
  const [saveState, setSaveState] = useState<'idle' | 'countdown' | 'saving'>('idle');
  const [countdown, setCountdown] = useState(5);
  const isInitialMount = useRef(true);

  useEffect(() => {
    if (isInitialMount.current) {
      isInitialMount.current = false;
      return;
    }
    setSaveState('countdown');
    setCountdown(5);
  }, [settings]);

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout>;

    if (saveState === 'countdown') {
      if (countdown > 0) {
        timer = setTimeout(() => { setCountdown((c) => c - 1); }, 1000);
      } else {
        timer = setTimeout(() => {
          setSaveState('saving');
          void useVPNStore.getState().saveSettings();
        }, 0);
      }
    } else if (saveState === 'saving') {
      timer = setTimeout(() => { setSaveState('idle'); }, 500);
    }

    return () => { clearTimeout(timer); };
  }, [saveState, countdown]);

  const handleManualSave = () => {
    if (saveState === 'countdown') {
      setSaveState('saving');
      void useVPNStore.getState().saveSettings();
    }
  };

  return (
    <div className="flex flex-col h-full w-full p-8 pt-6">
      <header className="mb-8 flex justify-between items-start gap-4">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">{t('systemConfig')}</h1>
          <p className="text-zinc-500 text-sm mt-1">{t('systemConfigDesc')}</p>
        </div>

        <button
          onClick={handleManualSave}
          disabled={saveState !== 'countdown'}
          className={`shrink-0 flex items-center justify-center gap-2 px-3 py-1.5 md:px-4 md:py-2 rounded-xl text-xs md:text-sm font-medium transition-colors border ${
            saveState === 'countdown'
              ? 'bg-blue-500/10 text-blue-400 border-blue-500/20 hover:bg-blue-500/20 cursor-pointer shadow-[0_0_15px_rgba(59,130,246,0.15)]'
              : 'bg-black/20 text-zinc-300 border-white/5 cursor-default'
          }`}
        >
          {saveState === 'saving' ? (
            <Loader2 size={16} className="animate-spin text-zinc-400" />
          ) : saveState === 'countdown' ? (
            <Timer size={16} className="animate-pulse" />
          ) : (
            <Check size={16} className="text-emerald-500" />
          )}

          <span className="hidden sm:inline">
            {saveState === 'saving'
              ? t('saving')
              : saveState === 'countdown'
              ? `${t('savingIn')} ${String(countdown)}s`
              : t('saved')}
          </span>
        </button>
      </header>

      <div className="mb-8">
        <SegmentedControl
          options={[
            { id: 'general', label: t('general'), icon: <Rocket size={16} /> },
            { id: 'network', label: t('network'), icon: <Network size={16} /> },
            { id: 'filtering', label: t('filtering'), icon: <Globe size={16} /> },
            { id: 'protocols', label: t('protocols'), icon: <Lock size={16} /> },
          ]}
          value={activeTab}
          onChange={(val) => {
            setActiveTab(val);
          }}
          layoutId="settingsTabIndicator"
        />
      </div>

      <div className="flex-1 overflow-y-auto overflow-x-hidden custom-scrollbar pb-4">
        <AnimatePresence mode="wait" initial={false}>
          <motion.div
            key={activeTab}
            initial={{ opacity: 0, x: -10 }}
            animate={{ opacity: 1, x: 0 }}
            exit={{ opacity: 0, x: 10 }}
            transition={{ duration: 0.15 }}
          >
            {activeTab === 'general' && <GeneralSettings />}
            {activeTab === 'network' && <NetworkSettings />}
            {activeTab === 'filtering' && <FilteringSettings />}
            {activeTab === 'protocols' && <ProtocolSettings />}
          </motion.div>
        </AnimatePresence>
      </div>
    </div>
  );
}

function GeneralSettings() {
  const { settings, updateSettings } = useVPNStore();
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
              updateSettings({ autoUpdateInterval: Number(e.target.value) });
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

function NetworkSettings() {
  const { settings, updateSettings } = useVPNStore();
  const { t } = useTranslation();
  return (
    <div className="space-y-4">
      <SettingItem
        icon={<ShieldAlert size={20} className="text-emerald-400" />}
        title={t('localityBypass')}
        description={t('localityBypassDesc')}
        checked={settings.bypassRu}
        onChange={(checked) => {
          updateSettings({ bypassRu: checked });
        }}
      />
      <SettingItem
        icon={<PowerOff size={20} className="text-red-400" />}
        title={t('killSwitch')}
        description={t('killSwitchDesc')}
        checked={settings.killSwitch}
        onChange={(checked) => {
          updateSettings({ killSwitch: checked });
        }}
      />
      <SettingItem
        icon={<Server size={20} className="text-purple-400" />}
        title={t('allowLan')}
        description={t('allowLanDesc')}
        checked={settings.allowLan}
        onChange={(checked) => {
          updateSettings({ allowLan: checked });
        }}
      />
      <div className="bg-white/5 border border-white/5 hover:bg-white/10 transition-colors rounded-2xl p-5 shadow-sm">
        <div className="flex items-center justify-between gap-6 mb-4">
          <div className="flex-1 flex gap-4">
            <div className="mt-1 bg-black/20 p-2 rounded-xl shadow-inner h-fit">
              <Network size={20} className="text-cyan-400" />
            </div>
            <div>
              <h3 className="font-medium text-sm text-zinc-200 tracking-wide">{t('customDns')}</h3>
              <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">{t('customDnsDesc')}</p>
            </div>
          </div>
          <button
            onClick={() => {
              const newVal = !settings.customDns;
              updateSettings({ customDns: newVal });
              toast(newVal ? t('enabled') : t('disabled'));
            }}
            className={`relative inline-flex h-6 w-11 items-center justify-center rounded-full transition-colors flex-shrink-0 focus:outline-none focus:ring-2 focus:ring-emerald-500/50 ${
              settings.customDns ? 'bg-emerald-500' : 'bg-zinc-700'
            }`}
          >
            <span
              className={`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${
                settings.customDns ? 'translate-x-2.5' : '-translate-x-2.5'
              }`}
            />
          </button>
        </div>
        {settings.customDns && (
          <div className="ml-14 animate-in fade-in slide-in-from-top-2">
            <input
              type="text"
              value={settings.dnsAddress}
              onChange={(e) => {
                updateSettings({ dnsAddress: e.target.value });
              }}
              placeholder="e.g. 1.1.1.1 or 8.8.8.8"
              className="w-full max-w-[250px] bg-black/20 border border-white/10 rounded-lg px-4 py-2 text-sm focus:border-emerald-500 focus:outline-none shadow-inner transition-colors"
            />
          </div>
        )}
      </div>
    </div>
  );
}

const DEFAULT_COUNTRIES = [
  { code: 'US', name: 'United States', flag: '🇺🇸' },
  { code: 'DE', name: 'Germany', flag: '🇩🇪' },
  { code: 'NL', name: 'Netherlands', flag: '🇳🇱' },
  { code: 'FR', name: 'France', flag: '🇫🇷' },
  { code: 'GB', name: 'United Kingdom', flag: '🇬🇧' },
  { code: 'TR', name: 'Turkey', flag: '🇹🇷' },
  { code: 'PL', name: 'Poland', flag: '🇵🇱' },
  { code: 'FI', name: 'Finland', flag: '🇫🇮' },
  { code: 'SG', name: 'Singapore', flag: '🇸🇬' },
  { code: 'JP', name: 'Japan', flag: '🇯🇵' },
];

function FilteringSettings() {
  const { settings, updateSettings } = useVPNStore();
  const { t } = useTranslation();

  const [newCode, setNewCode] = useState('');
  const [newName, setNewName] = useState('');
  const [newFlag, setNewFlag] = useState('');

  const availableCountries = [...DEFAULT_COUNTRIES, ...settings.customCountries];

  const handleToggleCountry = (countryCode: string, listType: 'allow' | 'block') => {
    const isAllowed = settings.allowedCountries.includes(countryCode);
    const isBlocked = settings.blockedCountries.includes(countryCode);

    let newAllowed = [...settings.allowedCountries];
    let newBlocked = [...settings.blockedCountries];

    if (listType === 'allow') {
      if (isBlocked) return;
      if (isAllowed) {
        newAllowed = newAllowed.filter((c) => c !== countryCode);
      } else {
        newAllowed.push(countryCode);
      }
    } else {
      if (isAllowed) return;
      if (isBlocked) {
        newBlocked = newBlocked.filter((c) => c !== countryCode);
      } else {
        newBlocked.push(countryCode);
      }
    }

    updateSettings({ allowedCountries: newAllowed, blockedCountries: newBlocked });
  };

  const addCustomCountry = () => {
    try {
      countrySchema.parse({ code: newCode, name: newName, flag: newFlag });
      const newCountry = { code: newCode.toUpperCase(), name: newName, flag: newFlag };
      updateSettings({ customCountries: [...settings.customCountries, newCountry] });
      setNewCode('');
      setNewName('');
      setNewFlag('');
      toast.success(t('countryAdded'));
    } catch (e: unknown) {
      if (e instanceof z.ZodError) {
        toast.error(e.issues[0]?.message || 'Invalid form data');
      } else {
        toast.error(e instanceof Error ? e.message : 'Error occurred');
      }
    }
  };

  return (
    <div className="space-y-6 pb-12">
      <div className="bg-white/5 border border-white/5 rounded-2xl p-6 shadow-sm">
        <h3 className="font-medium text-sm text-zinc-200 tracking-wide mb-1">
          {t('geoNodeRules')}
        </h3>
        <p className="text-xs text-zinc-500 mb-6">{t('geoNodeRulesDesc')}</p>

        <div className="space-y-8">
          <div>
            <h4 className="font-medium text-emerald-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-emerald-400" /> {t('allowedList')}
            </h4>
            <div className="flex flex-wrap gap-2">
              {availableCountries.map((country) => {
                const isSelected = settings.allowedCountries.includes(country.code);
                const isBlocked = settings.blockedCountries.includes(country.code);
                return (
                  <button
                    key={country.code}
                    disabled={isBlocked}
                    onClick={() => {
                      handleToggleCountry(country.code, 'allow');
                    }}
                    className={`flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all
                       ${isSelected ? 'bg-emerald-500/20 border-emerald-500/50 text-white shadow-inner' : 'bg-black/30 border-white/5 text-zinc-500 hover:text-zinc-300 hover:bg-black/50'}
                       ${isBlocked ? 'opacity-30 cursor-not-allowed' : ''}
                     `}
                  >
                    <span className="text-lg">{country.flag}</span>
                    {country.name}
                    {isBlocked && (
                      <span className="text-[10px] text-zinc-600 block">{t('alreadyInOther')}</span>
                    )}
                  </button>
                );
              })}
            </div>
            {settings.allowedCountries.length === 0 && (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyList')}</p>
            )}
          </div>

          <div>
            <h4 className="font-medium text-red-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-red-400" /> {t('blockedList')}
            </h4>
            <div className="flex flex-wrap gap-2">
              {availableCountries.map((country) => {
                const isSelected = settings.blockedCountries.includes(country.code);
                const isAllowed = settings.allowedCountries.includes(country.code);
                return (
                  <button
                    key={country.code}
                    disabled={isAllowed}
                    onClick={() => {
                      handleToggleCountry(country.code, 'block');
                    }}
                    className={`flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all
                       ${isSelected ? 'bg-red-500/20 border-red-500/50 text-white shadow-inner' : 'bg-black/30 border-white/5 text-zinc-500 hover:text-zinc-300 hover:bg-black/50'}
                       ${isAllowed ? 'opacity-30 cursor-not-allowed' : ''}
                     `}
                  >
                    <span className="text-lg">{country.flag}</span>
                    {country.name}
                    {isAllowed && (
                      <span className="text-[10px] text-zinc-600 block">{t('alreadyInOther')}</span>
                    )}
                  </button>
                );
              })}
            </div>
            {settings.blockedCountries.length === 0 && (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyList')}</p>
            )}
          </div>
        </div>

        <div className="mt-8 pt-6 border-t border-white/5">
          <h4 className="font-medium text-zinc-300 text-sm mb-3">{t('addCountry')}</h4>
          <div className="flex gap-2">
            <input
              type="text"
              value={newCode}
              onChange={(e) => {
                setNewCode(e.target.value);
              }}
              placeholder={t('countryCode')}
              className="w-20 bg-black/20 border border-white/10 rounded-lg px-3 py-2 text-sm focus:border-emerald-500 outline-none"
            />
            <input
              type="text"
              value={newName}
              onChange={(e) => {
                setNewName(e.target.value);
              }}
              placeholder={t('countryName')}
              className="flex-1 bg-black/20 border border-white/10 rounded-lg px-3 py-2 text-sm focus:border-emerald-500 outline-none"
            />
            <input
              type="text"
              value={newFlag}
              onChange={(e) => {
                setNewFlag(e.target.value);
              }}
              placeholder={t('countryFlag')}
              className="w-24 bg-black/20 border border-white/10 rounded-lg px-3 py-2 text-sm focus:border-emerald-500 outline-none text-center"
            />
            <button
              onClick={addCustomCountry}
              disabled={!newCode || !newName || !newFlag}
              className="bg-white/10 hover:bg-white/20 disabled:opacity-50 disabled:hover:bg-white/10 p-2 rounded-lg border border-white/10 text-white transition-colors"
            >
              <Plus size={20} />
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

const DEFAULT_PROTOCOLS = [
  { code: 'VLESS' },
  { code: 'VMess' },
  { code: 'Trojan' },
  { code: 'Shadowsocks' },
  { code: 'Socks' },
  { code: 'HTTP(S)' },
  { code: 'WireGuard' },
  { code: 'Hysteria2' },
  { code: 'TUIC' },
];

function ProtocolSettings() {
  const { settings, updateSettings } = useVPNStore();
  const { t } = useTranslation();

  const availableProtocols = DEFAULT_PROTOCOLS;

  const handleToggleProtocol = (code: string, listType: 'allow' | 'block') => {
    const isAllowed = settings.allowedProtocols.includes(code);
    const isBlocked = settings.blockedProtocols.includes(code);

    let newAllowed = [...settings.allowedProtocols];
    let newBlocked = [...settings.blockedProtocols];

    if (listType === 'allow') {
      if (isBlocked) return;
      if (isAllowed) {
        newAllowed = newAllowed.filter((p) => p !== code);
      } else {
        newAllowed.push(code);
      }
    } else {
      if (isAllowed) return;
      if (isBlocked) {
        newBlocked = newBlocked.filter((p) => p !== code);
      } else {
        newBlocked.push(code);
      }
    }

    updateSettings({ allowedProtocols: newAllowed, blockedProtocols: newBlocked });
  };

  return (
    <div className="space-y-6 pb-12">
      <div className="bg-white/5 border border-white/5 rounded-2xl p-6 shadow-sm">
        <h3 className="font-medium text-sm text-zinc-200 tracking-wide mb-1">
          {t('protocolRules')}
        </h3>
        <p className="text-xs text-zinc-500 mb-6">{t('protocolRulesDesc')}</p>

        <div className="space-y-8">
          <div>
            <h4 className="font-medium text-emerald-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-emerald-400" /> {t('protocolAllowedList')}
            </h4>
            <div className="flex flex-wrap gap-2">
              {availableProtocols.map((protocol) => {
                const isSelected = settings.allowedProtocols.includes(protocol.code);
                const isBlocked = settings.blockedProtocols.includes(protocol.code);
                return (
                  <button
                    key={protocol.code}
                    disabled={isBlocked}
                    onClick={() => {
                      handleToggleProtocol(protocol.code, 'allow');
                    }}
                    className={`flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all
                       ${isSelected ? 'bg-emerald-500/20 border-emerald-500/50 text-white shadow-inner' : 'bg-black/30 border-white/5 text-zinc-500 hover:text-zinc-300 hover:bg-black/50'}
                       ${isBlocked ? 'opacity-30 cursor-not-allowed' : ''}
                     `}
                  >
                    {protocol.code}
                    {isBlocked && (
                      <span className="text-[10px] text-zinc-600 block">{t('alreadyInOther')}</span>
                    )}
                  </button>
                );
              })}
            </div>
            {settings.allowedProtocols.length === 0 && (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyProtocolList')}</p>
            )}
          </div>

          <div>
            <h4 className="font-medium text-red-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-red-400" /> {t('protocolBlockedList')}
            </h4>
            <div className="flex flex-wrap gap-2">
              {availableProtocols.map((protocol) => {
                const isSelected = settings.blockedProtocols.includes(protocol.code);
                const isAllowed = settings.allowedProtocols.includes(protocol.code);
                return (
                  <button
                    key={protocol.code}
                    disabled={isAllowed}
                    onClick={() => {
                      handleToggleProtocol(protocol.code, 'block');
                    }}
                    className={`flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all
                       ${isSelected ? 'bg-red-500/20 border-red-500/50 text-white shadow-inner' : 'bg-black/30 border-white/5 text-zinc-500 hover:text-zinc-300 hover:bg-black/50'}
                       ${isAllowed ? 'opacity-30 cursor-not-allowed' : ''}
                     `}
                  >
                    {protocol.code}
                    {isAllowed && (
                      <span className="text-[10px] text-zinc-600 block">{t('alreadyInOther')}</span>
                    )}
                  </button>
                );
              })}
            </div>
            {settings.blockedProtocols.length === 0 && (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyProtocolList')}</p>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function SettingItem({
  icon,
  title,
  description,
  checked,
  onChange,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  const { t } = useTranslation();
  const handleChange = (v: boolean) => {
    onChange(v);
    if (v) {
      toast.success(`${title} ${t('enabled')}`);
    } else {
      toast(`${title} ${t('disabled')}`);
    }
  };

  return (
    <div className="bg-white/5 border border-white/5 hover:bg-white/10 transition-colors rounded-2xl p-5 flex items-center justify-between gap-6 shadow-sm">
      <div className="flex-1 flex gap-4">
        <div className="mt-1 bg-black/20 p-2 rounded-xl shadow-inner h-fit">{icon}</div>
        <div>
          <h3 className="font-medium text-sm text-zinc-200 tracking-wide">{title}</h3>
          <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">{description}</p>
        </div>
      </div>
      <button
        onClick={() => {
          handleChange(!checked);
        }}
        className={`relative inline-flex h-6 w-11 items-center justify-center rounded-full transition-colors flex-shrink-0 focus:outline-none focus:ring-2 focus:ring-emerald-500/50 ${
          checked ? 'bg-emerald-500' : 'bg-zinc-700'
        }`}
      >
        <span
          className={`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${
            checked ? 'translate-x-2.5' : '-translate-x-2.5'
          }`}
        />
      </button>
    </div>
  );
}
