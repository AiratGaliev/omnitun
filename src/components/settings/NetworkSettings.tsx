import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { Network, PowerOff, Server, ShieldAlert } from 'lucide-react';
import { toast } from 'sonner';
import { SettingItem } from './SettingItem';

export function NetworkSettings() {
  const { settings, updateSettings, addLog } = useVPNStore();
  const { t } = useTranslation();
  return (
    <div className="space-y-4">
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
      <div className="bg-white/5 border border-white/5 hover:bg-white/10 transition-colors rounded-2xl p-5 shadow-sm">
        <div className="flex items-center justify-between gap-6 mb-4">
          <div className="flex-1 flex gap-4">
            <div className="mt-1 bg-black/20 p-2 rounded-xl shadow-inner h-fit">
              <Network size={20} className="text-cyan-400" />
            </div>
            <div>
              <h3 className="font-medium text-sm text-zinc-200 tracking-wide">{t('customDns')}</h3>
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
            className={`relative inline-flex h-6 w-11 items-center justify-center rounded-full transition-colors flex-shrink-0 focus:outline-none focus:ring-2 focus:ring-emerald-500/50 ${
              settings.customDns ? 'bg-emerald-500' : 'bg-zinc-700'
            }`}
          >
            <span
              className={`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${
                settings.customDns ? 'translate-x-2.5' : '-translate-x-2.5'
              }`}
            />
          </button>
        </div>
        {settings.customDns && (
          <div className="ml-14 animate-in fade-in slide-in-from-top-2">
            <input
              type="text"
              value={settings.dnsAddress}
              onChange={(e) => {
                updateSettings({ dnsAddress: e.target.value });
              }}
              placeholder="e.g. 1.1.1.1 or 8.8.8.8"
              className="w-full max-w-[250px] bg-black/20 border border-white/10 rounded-lg px-4 py-2 text-sm focus:border-emerald-500 focus:outline-none shadow-inner transition-colors"
            />
          </div>
        )}
      </div>
    </div>
  );
}
