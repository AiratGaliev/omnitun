import { motion } from 'framer-motion';
import type { ReactNode } from 'react';

export interface SegmentedControlOption<T extends string> {
  id: T;
  label: string;
  icon?: ReactNode;
}

interface SegmentedControlProps<T extends string> {
  readonly options: SegmentedControlOption<T>[];
  readonly value: T;
  readonly onChange: (value: T) => void;
  readonly disabled?: boolean;
  readonly layoutId?: string;
}

export function SegmentedControl<T extends string>({
  options,
  value,
  onChange,
  disabled,
  layoutId = 'segmentedControlIndicator',
}: SegmentedControlProps<T>) {
  return (
    <div className="flex w-full bg-zinc-900/60 border border-white/5 rounded-2xl p-1.5 relative backdrop-blur-md shadow-inner">
      {options.map((option) => (
        <button
          key={option.id}
          onClick={() => { onChange(option.id); }}
          disabled={disabled}
          className={`flex-1 flex items-center justify-center gap-2 py-2.5 px-4 rounded-xl text-sm font-semibold transition-colors relative z-10 ${
            value === option.id ? 'text-white drop-shadow-sm' : 'text-zinc-500 hover:text-zinc-300'
          } disabled:opacity-50`}
        >
          {value === option.id && (
            <motion.div
              layoutId={layoutId}
              initial={false}
              transition={{ type: 'spring', stiffness: 400, damping: 30 }}
              className="absolute inset-0 bg-zinc-700/50 rounded-xl shadow-md border border-white/10 -z-10"
            />
          )}
          {option.icon && <span className="shrink-0">{option.icon}</span>}
          <span className="whitespace-nowrap">{option.label}</span>
        </button>
      ))}
    </div>
  );
}
