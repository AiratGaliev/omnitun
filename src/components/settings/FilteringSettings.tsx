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

  const hasAllowed = settings.allowedCountries.length > 0;
  const hasBlocked = settings.blockedCountries.length > 0;

  const handleToggleCountry = (countryCode: string, listType: 'allow' | 'block') => {
    let newAllowed = [...settings.allowedCountries];
    let newBlocked = [...settings.blockedCountries];

    if (listType === 'allow') {
      if (hasBlocked) return;
      const isAllowed = settings.allowedCountries.includes(countryCode);
      if (isAllowed) {
        newAllowed = newAllowed.filter((c) => c !== countryCode);
      } else {
        newAllowed.push(countryCode);
      }
    } else {
      if (hasAllowed) return;
      const isBlocked = settings.blockedCountries.includes(countryCode);
      if (isBlocked) {
        newBlocked = newBlocked.filter((c) => c !== countryCode);
      } else {
        newBlocked.push(countryCode);
      }
    }

    updateSettings({ allowedCountries: newAllowed, blockedCountries: newBlocked });
    const action =
      listType === 'allow'
        ? settings.allowedCountries.includes(countryCode) ? 'removed from allowed' : 'added to allowed'
        : settings.blockedCountries.includes(countryCode) ? 'removed from blocked' : 'added to blocked';
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
    <div className="space-y-4">
      <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-2xl p-6 shadow-sm">
        <h3 className="font-semibold text-sm text-zinc-100 tracking-wide mb-1">{t('geoNodeRules')}</h3>
        <p className="text-xs text-zinc-500 mb-6">{t('geoNodeRulesDesc')}</p>

        <div className="space-y-8">
          <div className={hasBlocked ? 'opacity-50 grayscale transition-all duration-300' : 'transition-all duration-300'}>
            <h4 className="font-medium text-emerald-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-emerald-400" />
              {t('allowedList')}
            </h4>
            <div className="flex flex-wrap gap-2">
              {availableCountries.map((country) => {
                const isSelected = settings.allowedCountries.includes(country.code);
                return (
                  <button
                    key={country.code}
                    disabled={hasBlocked}
                    onClick={() => { handleToggleCountry(country.code, 'allow'); }}
                    className={`flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all
                      ${isSelected ? 'bg-emerald-500/20 border-emerald-500/50 text-white shadow-inner' : 'bg-zinc-900/60 border-white/5 text-zinc-500 hover:text-zinc-300 hover:bg-zinc-800/60'}
                      ${hasBlocked ? 'cursor-not-allowed' : ''}`}
                  >
                    <span className="text-lg">{country.flag}</span>
                    {country.name}
                  </button>
                );
              })}
            </div>
            {settings.allowedCountries.length === 0 && !hasBlocked && (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyList')}</p>
            )}
            {hasBlocked && (
              <p className="text-xs text-zinc-500 mt-2">{t('alreadyInOther')}</p>
            )}
          </div>

          <div className={hasAllowed ? 'opacity-50 grayscale transition-all duration-300' : 'transition-all duration-300'}>
            <h4 className="font-medium text-red-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-red-400" />
              {t('blockedList')}
            </h4>
            <div className="flex flex-wrap gap-2">
              {availableCountries.map((country) => {
                const isSelected = settings.blockedCountries.includes(country.code);
                return (
                  <button
                    key={country.code}
                    disabled={hasAllowed}
                    onClick={() => { handleToggleCountry(country.code, 'block'); }}
                    className={`flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all
                      ${isSelected ? 'bg-red-500/20 border-red-500/50 text-white shadow-inner' : 'bg-zinc-900/60 border-white/5 text-zinc-500 hover:text-zinc-300 hover:bg-zinc-800/60'}
                      ${hasAllowed ? 'cursor-not-allowed' : ''}`}
                  >
                    <span className="text-lg">{country.flag}</span>
                    {country.name}
                  </button>
                );
              })}
            </div>
            {settings.blockedCountries.length === 0 && !hasAllowed && (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyList')}</p>
            )}
            {hasAllowed && (
              <p className="text-xs text-zinc-500 mt-2">{t('alreadyInOther')}</p>
            )}
          </div>
        </div>

        <div className="mt-8 pt-6 border-t border-white/5">
          <h4 className="font-medium text-zinc-300 text-sm mb-3">{t('addCountry')}</h4>
          <div className="flex gap-2">
            <input
              type="text"
              value={newCode}
              onChange={(e) => { setNewCode(e.target.value); }}
              placeholder={t('countryCode')}
              className="w-20 bg-zinc-950/50 border border-white/10 rounded-xl px-3 py-2.5 text-sm focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none transition-all shadow-inner"
            />
            <input
              type="text"
              value={newName}
              onChange={(e) => { setNewName(e.target.value); }}
              placeholder={t('countryName')}
              className="flex-1 bg-zinc-950/50 border border-white/10 rounded-xl px-3 py-2.5 text-sm focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none transition-all shadow-inner"
            />
            <input
              type="text"
              value={newFlag}
              onChange={(e) => { setNewFlag(e.target.value); }}
              placeholder={t('countryFlag')}
              className="w-24 bg-zinc-950/50 border border-white/10 rounded-xl px-3 py-2.5 text-sm focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none transition-all shadow-inner text-center"
            />
            <button
              onClick={addCustomCountry}
              disabled={!newCode || !newName || !newFlag}
              className="bg-zinc-800/80 hover:bg-zinc-700/80 disabled:opacity-50 disabled:hover:bg-zinc-800/80 p-2.5 rounded-xl border border-white/10 text-white transition-all shadow-sm"
            >
              <Plus size={20} />
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
