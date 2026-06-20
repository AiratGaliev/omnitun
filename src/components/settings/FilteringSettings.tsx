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
      if (isAllowed) newAllowed = newAllowed.filter((c) => c !== countryCode);
      else newAllowed.push(countryCode);
    } else {
      if (isAllowed) return;
      if (isBlocked) newBlocked = newBlocked.filter((c) => c !== countryCode);
      else newBlocked.push(countryCode);
    }

    updateSettings({ allowedCountries: newAllowed, blockedCountries: newBlocked });
    addLog(`Country ${countryCode} updated`, 'info', 'settings');
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
    } catch {
      toast.error('Invalid form data');
    }
  };

  return (
    <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-8 shadow-lg">
      <h3 className="font-semibold text-sm text-zinc-100 tracking-wide mb-1">{t('geoNodeRules')}</h3>
      <p className="text-xs text-zinc-500 mb-8">{t('geoNodeRulesDesc')}</p>

      <div className="space-y-8">
        <div>
          <h4 className="font-semibold text-emerald-400 text-sm mb-4 flex items-center gap-2">
            <div className="w-2 h-2 rounded-full bg-emerald-400 shadow-[0_0_8px_rgba(16,185,129,0.8)]" />
            {t('allowedList')}
          </h4>
          <div className="flex flex-wrap gap-2.5">
            {availableCountries.map((country) => {
              const isSelected = settings.allowedCountries.includes(country.code);
              const isBlocked = settings.blockedCountries.includes(country.code);
              return (
                <button
                  key={country.code}
                  disabled={isBlocked}
                  onClick={() => { handleToggleCountry(country.code, 'allow'); }}
                  className={`flex items-center gap-2 px-4 py-2.5 rounded-xl border text-sm font-medium transition-all duration-300
                    ${isSelected ? 'bg-emerald-500/20 border-emerald-500/40 text-emerald-300 shadow-inner' : 'bg-zinc-950/50 border-white/5 text-zinc-400 hover:text-zinc-200 hover:bg-zinc-800/80'}
                    ${isBlocked ? 'opacity-30 cursor-not-allowed' : ''}`}
                >
                  <span className="text-lg drop-shadow-sm">{country.flag}</span>
                  {country.name}
                </button>
              );
            })}
          </div>
        </div>

        <div>
          <h4 className="font-semibold text-red-400 text-sm mb-4 flex items-center gap-2">
            <div className="w-2 h-2 rounded-full bg-red-400 shadow-[0_0_8px_rgba(239,68,68,0.8)]" />
            {t('blockedList')}
          </h4>
          <div className="flex flex-wrap gap-2.5">
            {availableCountries.map((country) => {
              const isSelected = settings.blockedCountries.includes(country.code);
              const isAllowed = settings.allowedCountries.includes(country.code);
              return (
                <button
                  key={country.code}
                  disabled={isAllowed}
                  onClick={() => { handleToggleCountry(country.code, 'block'); }}
                  className={`flex items-center gap-2 px-4 py-2.5 rounded-xl border text-sm font-medium transition-all duration-300
                    ${isSelected ? 'bg-red-500/20 border-red-500/40 text-red-300 shadow-inner' : 'bg-zinc-950/50 border-white/5 text-zinc-400 hover:text-zinc-200 hover:bg-zinc-800/80'}
                    ${isAllowed ? 'opacity-30 cursor-not-allowed' : ''}`}
                >
                  <span className="text-lg drop-shadow-sm">{country.flag}</span>
                  {country.name}
                </button>
              );
            })}
          </div>
        </div>
      </div>

      <div className="mt-10 pt-8 border-t border-white/5">
        <h4 className="font-semibold text-zinc-300 text-sm mb-4">{t('addCountry')}</h4>
        <div className="flex gap-3">
          <input
            type="text" value={newCode} onChange={(e) => { setNewCode(e.target.value); }} placeholder={t('countryCode')}
            className="w-24 bg-zinc-950/50 border border-white/10 rounded-xl px-4 py-2.5 text-sm font-medium focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none transition-all shadow-inner"
          />
          <input
            type="text" value={newName} onChange={(e) => { setNewName(e.target.value); }} placeholder={t('countryName')}
            className="flex-1 bg-zinc-950/50 border border-white/10 rounded-xl px-4 py-2.5 text-sm font-medium focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none transition-all shadow-inner"
          />
          <input
            type="text" value={newFlag} onChange={(e) => { setNewFlag(e.target.value); }} placeholder={t('countryFlag')}
            className="w-24 bg-zinc-950/50 border border-white/10 rounded-xl px-4 py-2.5 text-sm font-medium focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none text-center transition-all shadow-inner"
          />
          <button
            onClick={addCustomCountry} disabled={!newCode || !newName || !newFlag}
            className="bg-zinc-800 hover:bg-zinc-700 disabled:opacity-50 disabled:hover:bg-zinc-800 px-4 rounded-xl border border-white/10 text-white transition-all shadow-sm"
          >
            <Plus size={20} />
          </button>
        </div>
      </div>
    </div>
  );
}
