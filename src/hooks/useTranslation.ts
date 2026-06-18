import { useVPNStore } from '../store';
import { translations, type TranslationKey } from '../i18n';

export function useTranslation() {
  const language = useVPNStore(state => state.settings.language);
  const t = (key: TranslationKey): string => {
    return translations[language as 'en' | 'ru'][key] || translations.en[key] || key;
  };
  return { t, lang: language };
}
