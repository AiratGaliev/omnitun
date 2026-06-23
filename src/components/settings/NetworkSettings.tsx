import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { Network, PowerOff, Server, Shield, ShieldAlert, Split } from 'lucide-react';
import { toast } from 'sonner';
import { SettingItem } from './SettingItem';

export function NetworkSettings() {
  const { settings, updateSettings, addLog } = useVPNStore();
  const { t } = useTranslation();
  return (
    <>
      <SettingItem
        icon={<ShieldAlert size={20} className="text-emerald-400" />}
        title={t('localityBypass')}
        description={t('localityBypassDesc')}
        checked={settings.bypassRu}
        onChange={(checked) => {
          updateSettings({ bypassRu: checked });
        }}
      />
      <SettingItem
        icon={<PowerOff size={20} className="text-red-400" />}
        title={t('killSwitch')}
        description={t('killSwitchDesc')}
        checked={settings.killSwitch}
        onChange={(checked) => {
          updateSettings({ killSwitch: checked });
        }}
      />
      <SettingItem
        icon={<Server size={20} className="text-purple-400" />}
        title={t('allowLan')}
        description={t('allowLanDesc')}
        checked={settings.allowLan}
        onChange={(checked) => {
          updateSettings({ allowLan: checked });
        }}
      />
      <div className="bg-zinc-900/40 backdrop-blur-md border border-white/5 hover:bg-zinc-900/60 hover:border-white/10 transition-all duration-300 rounded-2xl p-6 shadow-sm group">
        <div className="flex items-center justify-between gap-6 mb-4">
          <div className="flex-1 flex gap-5">
            <div className="mt-0.5 bg-zinc-800/80 p-2.5 rounded-xl shadow-inner h-fit border border-white/5 group-hover:border-white/10 transition-colors">
              <Network size={20} className="text-cyan-400" />
            </div>
            <div>
              <h3 className="font-semibold text-sm text-zinc-100 tracking-wide">
                {t('customDns')}
              </h3>
              <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">{t('customDnsDesc')}</p>
            </div>
          </div>
          <button
            onClick={() => {
              const newVal = !settings.customDns;
              updateSettings({ customDns: newVal });
              addLog(`Custom DNS ${newVal ? 'enabled' : 'disabled'}`, 'info', 'settings');
              toast(newVal ? t('enabled') : t('disabled'));
            }}
            className={`relative inline-flex h-7 w-12 items-center justify-center rounded-full transition-colors duration-300 shrink-0 focus:outline-none focus:ring-2 focus:ring-emerald-500/50 ${
              settings.customDns
                ? 'bg-emerald-500 shadow-[0_0_10px_rgba(16,185,129,0.4)]'
                : 'bg-zinc-700'
            }`}
          >
            <span
              className={`inline-block h-5 w-5 transform rounded-full bg-white shadow-sm transition-transform duration-300 ease-out ${settings.customDns ? 'translate-x-2.5' : '-translate-x-2.5'}`}
            />
          </button>
        </div>
        {settings.customDns && (
          <div className="ml-16 animate-in fade-in slide-in-from-top-2">
            <input
              type="text"
              value={settings.dnsAddress}
              onChange={(e) => {
                updateSettings({ dnsAddress: e.target.value });
              }}
              placeholder="e.g. 1.1.1.1 or 8.8.8.8"
              className="w-full max-w-62.5 bg-zinc-950/50 border border-white/10 rounded-xl px-4 py-2.5 text-sm font-mono text-zinc-300 focus:border-emerald-500/50 focus:ring-2 focus:ring-emerald-500/30 focus:outline-none shadow-inner transition-all"
            />
          </div>
        )}
      </div>
      <SettingItem
        icon={<Shield size={20} className="text-orange-400" />}
        title={t('antiDpi')}
        description={t('antiDpiDesc')}
        checked={settings.antiDpi}
        onChange={(checked) => {
          updateSettings({ antiDpi: checked });
          addLog(`Anti-DPI ${checked ? 'enabled' : 'disabled'}`, 'info', 'settings');
        }}
      />
      <SettingItem
        icon={<Split size={20} className="text-pink-400" />}
        title={t('enableMux')}
        description={t('enableMuxDesc')}
        checked={settings.enableMux}
        onChange={(checked) => {
          updateSettings({ enableMux: checked });
          addLog(`Mux ${checked ? 'enabled' : 'disabled'}`, 'info', 'settings');
        }}
      />
    </>
  );
}
