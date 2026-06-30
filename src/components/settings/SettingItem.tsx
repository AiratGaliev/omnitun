import { type ReactNode } from 'react';
import { toast } from 'sonner';
import { useVPNStore } from '@/store.ts';
import { useTranslation } from '@/hooks/useTranslation.ts';

interface SettingItemProps {
  readonly icon: ReactNode;
  readonly title: string;
  readonly description: string;
  readonly checked: boolean;
  readonly disabled?: boolean;
  readonly onChange: (checked: boolean) => void;
}

export function SettingItem({ icon, title, description, checked, disabled, onChange }: SettingItemProps) {
  const { t } = useTranslation();
  const addLog = useVPNStore((state) => state.addLog);

  const handleChange = (v: boolean) => {
    if (disabled) return;
    onChange(v);
    addLog(`"${title}" ${v ? 'enabled' : 'disabled'}`, 'info', 'settings');
    if (v) toast.success(`${title} ${t('enabled')}`);
    else toast(`${title} ${t('disabled')}`);
  };

  return (
    <div className={`bg-zinc-900/40 backdrop-blur-md border border-white/5 rounded-2xl p-6 flex items-center justify-between gap-6 shadow-sm group
      ${disabled ? 'opacity-50 grayscale transition-all duration-300' : 'hover:bg-zinc-900/60 hover:border-white/10 transition-all duration-300'}`}>
      <div className="flex-1 flex gap-5">
        <div className="mt-0.5 bg-zinc-800/80 p-2.5 rounded-xl shadow-inner h-fit border border-white/5 group-hover:border-white/10 transition-colors">
          {icon}
        </div>
        <div>
          <h3 className="font-semibold text-sm text-zinc-100 tracking-wide">{title}</h3>
          <p className="text-xs text-zinc-500 mt-1.5 leading-relaxed max-w-xl">{description}</p>
        </div>
      </div>
      <button
        onClick={() => {
          handleChange(!checked);
        }}
        disabled={disabled}
        className={`relative inline-flex h-7 w-12 items-center justify-center rounded-full transition-colors duration-300 flex-shrink-0 focus:outline-none focus:ring-2 focus:ring-emerald-500/50 focus:ring-offset-2 focus:ring-offset-zinc-950 ${
          checked ? 'bg-emerald-500 shadow-[0_0_10px_rgba(16,185,129,0.4)]' : 'bg-zinc-700'
        } ${disabled ? 'cursor-not-allowed' : 'cursor-pointer'}`}
      >
        <span
          className={`inline-block h-5 w-5 transform rounded-full bg-white shadow-sm transition-transform duration-300 ease-out ${
            checked ? 'translate-x-2.5' : '-translate-x-2.5'
          }`}
        />
      </button>
    </div>
  );
}
