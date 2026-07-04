// src/lib/errors.ts
//
// Error localization and message extraction utilities.
//
// The Rust ErrorCode enum is serialized as camelCase (via serde), so each code
// (e.g. "subscriptionDuplicateUrl") matches an i18n key directly. We use
// i18next's `defaultValue` option to fall back to the English dev message
// from Rust when no translation exists — no manual string comparison needed.

import { isRustError } from '@/types';
import { i18n } from '@/i18n';
import type { TranslationKey } from '@/i18n';

/**
 * Extracts a developer-facing message (English) from any error.
 * Use this for logs — it always returns a readable string, never
 * "[object Object]".
 *
 * - RustError → err.message (English dev text from Rust)
 * - Error → err.message
 * - string → the string itself
 * - anything else → JSON.stringify (or String() as last resort)
 */
export function getErrorMessage(err: unknown): string {
  if (isRustError(err)) return err.message;
  if (err instanceof Error) return err.message;
  if (typeof err === 'string') return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

/**
 * Returns a user-facing localized message for any error.
 *
 * - If err is a RustError:
 *     Looks up err.code in i18n. If a translation exists, returns it.
 *     If not (e.g. backend was updated but frontend i18n wasn't), returns
 *     err.message (English dev text from Rust) via i18next's `defaultValue`.
 * - Otherwise (string, Error, unknown):
 *     Returns the translation of `fallbackKey` (defaults to 'addFailed').
 *
 * REACT COMPATIBILITY:
 *   This function uses the global `i18n.t` directly, which does NOT trigger
 *   React re-renders on language change. This is fine for toast notifications
 *   (which are ephemeral and don't need to re-render). For inline error
 *   messages in components that must update on language change, use
 *   useTranslation().t(err.code, { defaultValue: err.message }) instead.
 *
 * USAGE:
 *   // In store.ts (non-React):
 *   toast.error(localizeError(e, 'connectionFailedDesc'));
 */
export function localizeError(err: unknown, fallbackKey: TranslationKey = 'addFailed'): string {
  if (isRustError(err)) {
    // i18n.t accepts any string. If key is missing, returns defaultValue.
    return i18n.t(err.code, { defaultValue: err.message });
  }
  return i18n.t(fallbackKey);
}
