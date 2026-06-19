import { type ReactNode } from 'react';
import { toast } from 'sonner';
import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';

interface SettingItemProps {
  readonly icon: ReactNode;
  readonly title: string;
  readonly description: string;
  readonly checked: boolean;
  readonly onChange: (checked: boolean) => void;
}

export function SettingItem({ icon, title, description, checked, onChange }: SettingItemProps) {
  const { t } = useTranslation();
  const addLog = useVPNStore((state) => state.addLog);
  const handleChange = (v: boolean) => {
    onChange(v);
    addLog(`"${title}" ${v ? 'enabled' : 'disabled'}`, 'info', 'settings');
    if (v) {
      toast.success(`${title} ${t('enabled')}`);
    } else {
      toast(`${title} ${t('disabled')}`);
    }
  };

  return (
    <div className="bg-white/5 border border-white/5 hover:bg-white/10 transition-colors rounded-2xl p-5 flex items-center justify-between gap-6 shadow-sm">
      <div className="flex-1 flex gap-4">
        <div className="mt-1 bg-black/20 p-2 rounded-xl shadow-inner h-fit">{icon}</div>
        <div>
          <h3 className="font-medium text-sm text-zinc-200 tracking-wide">{title}</h3>
          <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed">{description}</p>
        </div>
      </div>
      <button
        onClick={() => {
          handleChange(!checked);
        }}
        className={`relative inline-flex h-6 w-11 items-center justify-center rounded-full transition-colors flex-shrink-0 focus:outline-none focus:ring-2 focus:ring-emerald-500/50 ${
          checked ? 'bg-emerald-500' : 'bg-zinc-700'
        }`}
      >
        <span
          className={`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${
            checked ? 'translate-x-2.5' : '-translate-x-2.5'
          }`}
        />
      </button>
    </div>
  );
}
