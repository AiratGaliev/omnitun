import { useVPNStore } from '@/store.ts';
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
  const { settings, updateSettings, addLog } = useVPNStore();
  const { t } = useTranslation();

  const handleToggleProtocol = (code: string, listType: 'allow' | 'block') => {
    const isAllowed = settings.allowedProtocols.includes(code);
    const isBlocked = settings.blockedProtocols.includes(code);

    let newAllowed = [...settings.allowedProtocols];
    let newBlocked = [...settings.blockedProtocols];

    if (listType === 'allow') {
      if (isBlocked) return;
      if (isAllowed) newAllowed = newAllowed.filter((p) => p !== code);
      else newAllowed.push(code);
    } else {
      if (isAllowed) return;
      if (isBlocked) newBlocked = newBlocked.filter((p) => p !== code);
      else newBlocked.push(code);
    }

    updateSettings({ allowedProtocols: newAllowed, blockedProtocols: newBlocked });
    addLog(`Protocol ${code} updated`, 'info', 'settings');
  };

  function ProtocolButton({ code, listType }: { code: string; listType: 'allow' | 'block' }) {
    const isAllow = listType === 'allow';
    const isSelected = isAllow ? settings.allowedProtocols.includes(code) : settings.blockedProtocols.includes(code);
    const isDisabled = isAllow ? settings.blockedProtocols.includes(code) : settings.allowedProtocols.includes(code);

    return (
      <button
        key={code}
        disabled={isDisabled}
        onClick={() => { handleToggleProtocol(code, listType); }}
        className={`flex items-center gap-2 px-4 py-2.5 rounded-xl border text-sm font-medium transition-all duration-300
          ${isSelected
              ? isAllow ? 'bg-emerald-500/20 border-emerald-500/40 text-emerald-300 shadow-inner' : 'bg-red-500/20 border-red-500/40 text-red-300 shadow-inner'
              : 'bg-zinc-950/50 border-white/5 text-zinc-400 hover:text-zinc-200 hover:bg-zinc-800/80'
          }
          ${isDisabled ? 'opacity-30 cursor-not-allowed' : ''}`}
      >
        {code}
      </button>
    );
  }

  return (
    <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-3xl p-8 shadow-lg">
      <h3 className="font-semibold text-sm text-zinc-100 tracking-wide mb-1">{t('protocolRules')}</h3>
      <p className="text-xs text-zinc-500 mb-8">{t('protocolRulesDesc')}</p>

      <div className="space-y-8">
        <div>
          <h4 className="font-semibold text-emerald-400 text-sm mb-4 flex items-center gap-2">
            <div className="w-2 h-2 rounded-full bg-emerald-400 shadow-[0_0_8px_rgba(16,185,129,0.8)]" />
            {t('protocolAllowedList')}
          </h4>
          <div className="flex flex-wrap gap-2.5">
            {DEFAULT_PROTOCOLS.map((p) => <ProtocolButton key={p.code} code={p.code} listType="allow" />)}
          </div>
        </div>

        <div>
          <h4 className="font-semibold text-red-400 text-sm mb-4 flex items-center gap-2">
            <div className="w-2 h-2 rounded-full bg-red-400 shadow-[0_0_8px_rgba(239,68,68,0.8)]" />
            {t('protocolBlockedList')}
          </h4>
          <div className="flex flex-wrap gap-2.5">
            {DEFAULT_PROTOCOLS.map((p) => <ProtocolButton key={p.code} code={p.code} listType="block" />)}
          </div>
        </div>
      </div>
    </div>
  );
}
