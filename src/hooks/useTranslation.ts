import { useTranslation as useI18nTranslation } from 'react-i18next';
import type { TranslationKey } from '@/i18n';

export function useTranslation() {
  const { t, i18n } = useI18nTranslation();
  return {
    t: (key: TranslationKey): string => t(key),
    lang: i18n.language,
  };
}
