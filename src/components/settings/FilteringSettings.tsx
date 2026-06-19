import { useState } from 'react';
import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { Plus } from 'lucide-react';
import { toast } from 'sonner';
import * as z from 'zod';

const countrySchema = z.object({
  code: z.string().min(2).max(3),
  name: z.string().min(2),
  flag: z.string().min(1),
});

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

export function FilteringSettings() {
  const { settings, updateSettings, addLog } = useVPNStore();
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
    const action =
      listType === 'allow'
        ? isAllowed
          ? 'removed from allowed'
          : 'added to allowed'
        : isBlocked
          ? 'removed from blocked'
          : 'added to blocked';
    addLog(`Country ${countryCode} ${action}`, 'info', 'settings');
  };

  const addCustomCountry = () => {
    try {
      countrySchema.parse({ code: newCode, name: newName, flag: newFlag });
      const newCountry = { code: newCode.toUpperCase(), name: newName, flag: newFlag };
      updateSettings({ customCountries: [...settings.customCountries, newCountry] });
      setNewCode('');
      setNewName('');
      setNewFlag('');
      addLog(`Custom country ${newCountry.code} (${newCountry.name}) added`, 'info', 'settings');
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
              <div className="w-2 h-2 rounded-full bg-emerald-400" />
              {t('allowedList')}
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
                      ${isBlocked ? 'opacity-30 cursor-not-allowed' : ''}`}
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
              <div className="w-2 h-2 rounded-full bg-red-400" />
              {t('blockedList')}
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
                      ${isAllowed ? 'opacity-30 cursor-not-allowed' : ''}`}
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
