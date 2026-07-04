import { useState, useMemo } from 'react';
import { useAppStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { CloudOff, Plus, Search, X } from 'lucide-react';
import { SettingItem } from './SettingItem';
import { getAllCountries } from '@/lib/countryNames';

export function FilteringSettings() {
  const { settings, updateSettings, addLog } = useAppStore();
  const { t, lang } = useTranslation();

  const [searchQuery, setSearchQuery] = useState('');

  /**
   * Get all ~250 countries localized via i18n-iso-countries (no hardcode).
   * Реактивен к смене языка: dep lang из useTranslation() обновляет список.
   */
  const isoCountries = useMemo(() => {
    return getAllCountries(lang).map(({ code, name }) => ({
      code,
      name,
      flag: code,
    }));
  }, [lang]);

  const availableCountries = useMemo(
    () => [...isoCountries, ...settings.customCountries],
    [isoCountries, settings.customCountries],
  );

  /**
   * Lookup a country by its code from the full available list.
   * Returns the full object (flag, name) or null if not found.
   */
  const lookupCountry = useMemo(() => {
    const map = new Map<string, { code: string; name: string; flag: string }>();
    for (const c of availableCountries) {
      map.set(c.code, c);
    }
    return map;
  }, [availableCountries]);

  const hasAllowed = settings.allowedCountries.length > 0;
  const hasBlocked = settings.blockedCountries.length > 0;

  /** Search results for the top quick-add search. */
  const filteredCountries = useMemo(() => {
    if (!searchQuery) return [];
    const q = searchQuery.toLowerCase();
    return availableCountries.filter(
      (c) => c.code.toLowerCase().includes(q) || c.name.toLowerCase().includes(q),
    );
  }, [availableCountries, searchQuery]);

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

    // Country filtering is meaningless for CDN servers (unstable exit IP).
    // Automatically enable blockCdnNodes when any country filter is set.
    const hasAnyCountry = newAllowed.length > 0 || newBlocked.length > 0;
    updateSettings({
      allowedCountries: newAllowed,
      blockedCountries: newBlocked,
      blockCdnNodes: hasAnyCountry,
    });
    const action =
      listType === 'allow'
        ? settings.allowedCountries.includes(countryCode)
          ? 'removed from allowed'
          : 'added to allowed'
        : settings.blockedCountries.includes(countryCode)
          ? 'removed from blocked'
          : 'added to blocked';
    addLog(`Country ${countryCode} ${action}`, 'info', 'settings');
  };

  /** Short action label for the search result buttons. */
  const btnText = {
    allow: t('allowedList').split(' ')[0],
    block: t('blockedList').split(' ')[0],
  };

  return (
    <div className="space-y-4">
      <SettingItem
        icon={<CloudOff size={20} className="text-blue-400" />}
        title={t('blockCdnNodes')}
        description={t('blockCdnNodesDesc')}
        checked={settings.blockCdnNodes}
        disabled={hasAllowed || hasBlocked}
        onChange={(checked) => {
          updateSettings({ blockCdnNodes: checked });
        }}
      />

      <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-2xl p-6 shadow-sm">
        <h3 className="font-semibold text-sm text-zinc-100 tracking-wide mb-1">
          {t('geoNodeRules')}
        </h3>
        <p className="text-xs text-zinc-500 mb-6">{t('geoNodeRulesDesc')}</p>

        <div className="space-y-8">
          {/* Dynamic add country — single search field */}
          <div className="pb-6 border-b border-white/5">
            <h4 className="font-medium text-zinc-300 text-sm mb-3">{t('addCountry')}</h4>
            <div className="relative">
              <Search
                size={16}
                className="absolute left-3 top-1/2 -translate-y-1/2 text-zinc-500"
              />
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => {
                  setSearchQuery(e.target.value);
                }}
                placeholder={t('searchCountry')}
                className="w-full bg-zinc-950/50 border border-white/10 rounded-xl pl-9 pr-3 py-2.5 text-sm focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none transition-all shadow-inner"
              />
            </div>
            {filteredCountries.length > 0 && (
              <div className="mt-4 max-h-60 overflow-y-auto custom-scrollbar space-y-1">
                {filteredCountries.map((country) => {
                  const isAllowed = settings.allowedCountries.includes(country.code);
                  const isBlocked = settings.blockedCountries.includes(country.code);
                  return (
                    <div
                      key={country.code}
                      className="flex items-center justify-between px-3 py-2 rounded-xl bg-zinc-900/60 border border-white/5"
                    >
                      <span className="flex items-center gap-2 text-sm text-zinc-300 min-w-0">
                        <span className="text-lg shrink-0">{country.flag}</span>
                        <span className="truncate">{country.name}</span>
                      </span>
                      <div className="flex items-center gap-1.5 shrink-0 ml-2">
                        <button
                          onClick={() => {
                            handleToggleCountry(country.code, 'allow');
                          }}
                          disabled={hasBlocked && !isAllowed}
                          className={`flex items-center gap-1 px-2.5 py-1 rounded-lg text-xs font-medium transition-all border
                            ${
                              isAllowed
                                ? 'bg-emerald-500/20 border-emerald-500/50 text-emerald-400 shadow-inner'
                                : 'bg-zinc-800/80 border-white/5 text-zinc-500 hover:text-emerald-400 hover:border-emerald-500/30'
                            }
                            ${hasBlocked && !isAllowed ? 'opacity-30 cursor-not-allowed' : ''}`}
                        >
                          <Plus size={12} strokeWidth={3} />
                          {btnText.allow}
                        </button>
                        <button
                          onClick={() => {
                            handleToggleCountry(country.code, 'block');
                          }}
                          disabled={hasAllowed && !isBlocked}
                          className={`flex items-center gap-1 px-2.5 py-1 rounded-lg text-xs font-medium transition-all border
                            ${
                              isBlocked
                                ? 'bg-red-500/20 border-red-500/50 text-red-400 shadow-inner'
                                : 'bg-zinc-800/80 border-white/5 text-zinc-500 hover:text-red-400 hover:border-red-500/30'
                            }
                            ${hasAllowed && !isBlocked ? 'opacity-30 cursor-not-allowed' : ''}`}
                        >
                          <Plus size={12} strokeWidth={3} />
                          {btnText.block}
                        </button>
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          <div
            className={
              hasBlocked
                ? 'opacity-50 grayscale transition-all duration-300'
                : 'transition-all duration-300'
            }
          >
            <h4 className="font-medium text-emerald-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-emerald-400" />
              {t('allowedList')}
              {hasAllowed && (
                <span className="text-[10px] text-emerald-500/60 font-mono">
                  ({settings.allowedCountries.length})
                </span>
              )}
            </h4>
            {/* Only added countries shown */}
            {hasAllowed ? (
              <div className="flex flex-wrap gap-2">
                {settings.allowedCountries.map((code) => {
                  const country = lookupCountry.get(code);
                  return (
                    <button
                      key={code}
                      onClick={() => {
                        handleToggleCountry(code, 'allow');
                      }}
                      className="flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all bg-emerald-500/20 border-emerald-500/50 text-white shadow-inner hover:bg-red-500/20 hover:border-red-500/50 group"
                    >
                      <span className="text-lg">{country?.flag ?? '🏳'}</span>
                      <span className="truncate max-w-32">{country?.name ?? code}</span>
                      <X
                        size={14}
                        className="shrink-0 opacity-0 group-hover:opacity-60 transition-opacity text-red-400"
                      />
                    </button>
                  );
                })}
              </div>
            ) : (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyList')}</p>
            )}
            {hasBlocked && <p className="text-xs text-zinc-500 mt-2">{t('alreadyInOther')}</p>}
          </div>

          <div
            className={
              hasAllowed
                ? 'opacity-50 grayscale transition-all duration-300'
                : 'transition-all duration-300'
            }
          >
            <h4 className="font-medium text-red-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-red-400" />
              {t('blockedList')}
              {hasBlocked && (
                <span className="text-[10px] text-red-500/60 font-mono">
                  ({settings.blockedCountries.length})
                </span>
              )}
            </h4>
            {/* Only added countries shown */}
            {hasBlocked ? (
              <div className="flex flex-wrap gap-2">
                {settings.blockedCountries.map((code) => {
                  const country = lookupCountry.get(code);
                  return (
                    <button
                      key={code}
                      onClick={() => {
                        handleToggleCountry(code, 'block');
                      }}
                      className="flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all bg-red-500/20 border-red-500/50 text-white shadow-inner hover:bg-emerald-500/20 hover:border-emerald-500/50 group"
                    >
                      <span className="text-lg">{country?.flag ?? '🏳'}</span>
                      <span className="truncate max-w-32">{country?.name ?? code}</span>
                      <X
                        size={14}
                        className="shrink-0 opacity-0 group-hover:opacity-60 transition-opacity text-emerald-400"
                      />
                    </button>
                  );
                })}
              </div>
            ) : (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyList')}</p>
            )}
            {hasAllowed && <p className="text-xs text-zinc-500 mt-2">{t('alreadyInOther')}</p>}
          </div>
        </div>
      </div>
    </div>
  );
}
