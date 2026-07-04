/**
 * Country code → country name conversion with i18n support.
 *
 * Uses i18n-iso-countries (https://github.com/michaelwittig/node-i18n-iso-countries)
 * — стабильный npm-пакет с поддержкой 79 языков, без хардкода.
 *
 * Бэкенд (Rust) возвращает ТОЛЬКО country_code ("US", "DE", "JP").
 * country_name может быть пустым, если:
 *   - ip.guide вернул только code (без name)
 *   - MaxMind City не имеет name для этого IP
 *
 * Этот helper заполняет country_name на фронте, с учётом locale пользователя.
 *
 * 🚫 ANTI-REGRESSION: не заменяй на хардкод-таблицу. i18n-iso-countries
 * поддерживает 79 языков и обновляется сообществом. Хардкод устаревает.
 */

import { registerLocale, getName, getNames } from 'i18n-iso-countries';
import enLocale from 'i18n-iso-countries/langs/en.json';
import ruLocale from 'i18n-iso-countries/langs/ru.json';

// Регистрируем только нужные локали (не тащим все 79).
registerLocale(enLocale);
registerLocale(ruLocale);

/**
 * Convert ISO 3166-1 alpha-2 country code to localized name.
 *
 * @example
 *   getCountryName('US', 'en') → 'United States of America'
 *   getCountryName('US', 'ru') → 'Соединённые Штаты Америки'
 *   getCountryName('XX', 'en') → 'XX' (unknown, return as-is)
 */
export function getCountryName(code: string | null | undefined, locale = 'en'): string {
  if (!code || code.length === 0) {
    return '';
  }
  const upperCode = code.toUpperCase();
  const supportedLocale = ['en', 'ru'].includes(locale) ? locale : 'en';
  return getName(upperCode, supportedLocale) ?? upperCode;
}

/**
 * Get list of all ~250 countries for UI selectors.
 * Replaces hardcoded DEFAULT_COUNTRIES in FilteringSettings.tsx.
 */
export function getAllCountries(locale = 'en'): { code: string; name: string }[] {
  const supportedLocale = ['en', 'ru'].includes(locale) ? locale : 'en';
  const allNames = getNames(supportedLocale);
  return Object.entries(allNames).map(([code, name]) => ({ code, name }));
}
