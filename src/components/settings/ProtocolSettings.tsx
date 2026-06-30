import { useAppStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';

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

export function ProtocolSettings() {
  const { settings, updateSettings, addLog } = useAppStore();
  const { t } = useTranslation();

  const hasAllowed = settings.allowedProtocols.length > 0;
  const hasBlocked = settings.blockedProtocols.length > 0;

  const handleToggleProtocol = (code: string, listType: 'allow' | 'block') => {
    let newAllowed = [...settings.allowedProtocols];
    let newBlocked = [...settings.blockedProtocols];

    if (listType === 'allow') {
      if (hasBlocked) return;
      const isAllowed = settings.allowedProtocols.includes(code);
      if (isAllowed) {
        newAllowed = newAllowed.filter((p) => p !== code);
      } else {
        newAllowed.push(code);
      }
    } else {
      if (hasAllowed) return;
      const isBlocked = settings.blockedProtocols.includes(code);
      if (isBlocked) {
        newBlocked = newBlocked.filter((p) => p !== code);
      } else {
        newBlocked.push(code);
      }
    }

    updateSettings({ allowedProtocols: newAllowed, blockedProtocols: newBlocked });
    const action =
      listType === 'allow'
        ? settings.allowedProtocols.includes(code)
          ? 'removed from allowed'
          : 'added to allowed'
        : settings.blockedProtocols.includes(code)
          ? 'removed from blocked'
          : 'added to blocked';
    addLog(`Protocol ${code} ${action}`, 'info', 'settings');
  };

  interface ProtocolButtonProps {
    code: string;
    listType: 'allow' | 'block';
  }

  function ProtocolButton({ code, listType }: ProtocolButtonProps) {
    const isAllow = listType === 'allow';
    const isSelected = isAllow
      ? settings.allowedProtocols.includes(code)
      : settings.blockedProtocols.includes(code);
    const isDisabled = isAllow ? hasBlocked : hasAllowed;

    return (
      <button
        key={code}
        disabled={isDisabled}
        onClick={() => {
          handleToggleProtocol(code, listType);
        }}
        className={`flex items-center gap-2 px-3 py-2 rounded-lg border text-sm transition-all
          ${
            isSelected
              ? isAllow
                ? 'bg-emerald-500/20 border-emerald-500/50 text-white shadow-inner'
                : 'bg-red-500/20 border-red-500/50 text-white shadow-inner'
              : 'bg-zinc-900/60 border-white/5 text-zinc-500 hover:text-zinc-300 hover:bg-zinc-800/60'
          }
          ${isDisabled ? 'cursor-not-allowed' : ''}`}
      >
        {code}
      </button>
    );
  }

  return (
    <div className="space-y-4">
      <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-2xl p-6 shadow-sm">
        <h3 className="font-semibold text-sm text-zinc-100 tracking-wide mb-1">
          {t('protocolRules')}
        </h3>
        <p className="text-xs text-zinc-500 mb-6">{t('protocolRulesDesc')}</p>

        <div className="space-y-8">
          <div
            className={
              hasBlocked
                ? 'opacity-50 grayscale transition-all duration-300'
                : 'transition-all duration-300'
            }
          >
            <h4 className="font-medium text-emerald-400 text-sm mb-3 flex items-center gap-2">
              <div className="w-2 h-2 rounded-full bg-emerald-400" />
              {t('protocolAllowedList')}
            </h4>
            <div className="flex flex-wrap gap-2">
              {DEFAULT_PROTOCOLS.map((p) => (
                <ProtocolButton key={p.code} code={p.code} listType="allow" />
              ))}
            </div>
            {settings.allowedProtocols.length === 0 && !hasBlocked && (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyProtocolList')}</p>
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
              {t('protocolBlockedList')}
            </h4>
            <div className="flex flex-wrap gap-2">
              {DEFAULT_PROTOCOLS.map((p) => (
                <ProtocolButton key={p.code} code={p.code} listType="block" />
              ))}
            </div>
            {settings.blockedProtocols.length === 0 && !hasAllowed && (
              <p className="text-xs text-zinc-600 mt-2 italic">{t('emptyProtocolList')}</p>
            )}
            {hasAllowed && <p className="text-xs text-zinc-500 mt-2">{t('alreadyInOther')}</p>}
          </div>
        </div>
      </div>
    </div>
  );
}
