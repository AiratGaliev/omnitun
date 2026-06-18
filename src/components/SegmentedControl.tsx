import { motion } from 'framer-motion';
import type { ReactNode } from 'react';

export interface SegmentedControlOption<T extends string> {
  id: T;
  label: string;
  icon?: ReactNode;
}

interface SegmentedControlProps<T extends string> {
  options: readonly SegmentedControlOption<T>[];
  value: T;
  onChange: (value: T) => void;
  disabled?: boolean;
  layoutId?: string;
}

export function SegmentedControl<T extends string>({
  options,
  value,
  onChange,
  disabled,
  layoutId = 'segmentedControlIndicator',
}: SegmentedControlProps<T>) {
  return (
    <div className="flex w-full bg-black/30 border border-white/5 rounded-xl p-1 relative overflow-x-auto custom-scrollbar">
      {options.map((option) => (
        <button
          key={option.id}
          onClick={() => {
            onChange(option.id);
          }}
          disabled={disabled}
          className={`flex-1 flex items-center justify-center gap-2 py-2 px-2 md:py-2.5 md:px-4 rounded-lg text-sm font-medium transition-colors relative z-10 ${
            value === option.id ? 'text-white' : 'text-zinc-500 hover:text-zinc-300'
          } disabled:opacity-50`}
        >
          {value === option.id && (
            <motion.div
              layoutId={layoutId}
              initial={false}
              transition={{ type: 'spring', stiffness: 300, damping: 30 }}
              className="absolute inset-0 bg-zinc-800 rounded-lg shadow-sm border border-white/5 -z-10"
            />
          )}
          {option.icon && <span className="shrink-0">{option.icon}</span>}
          <span className="whitespace-nowrap">{option.label}</span>
        </button>
      ))}
    </div>
  );
}
