import { useTranslation as useI18nTranslation } from 'react-i18next';
import type { TranslationKey } from '@/i18n';

/**
 * Extended translation hook.
 *
 * `t` accepts:
 *   - `TranslationKey` — known i18n keys (full IDE autocomplete, type-safe)
 *   - `string & {}` — dynamic keys (e.g. Rust error codes like
 *     "subscriptionDuplicateUrl") — the `& {}` trick preserves autocomplete
 *     for TranslationKey while allowing arbitrary strings without `as` cast
 *
 * Optional `options` passed through to i18next (e.g. `{ defaultValue: ... }`).
 */
export function useTranslation() {
  const { t, i18n } = useI18nTranslation();
  return {
    t: (key: TranslationKey | (string & {}), options?: Record<string, unknown>): string =>
      t(key, options),
    lang: i18n.language,
  };
}
