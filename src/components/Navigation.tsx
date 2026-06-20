import { Home, Info, List, Settings, Terminal } from 'lucide-react';
import { motion } from 'framer-motion';
import { useTranslation } from '../hooks/useTranslation';

interface NavigationProps {
  readonly activeTab: 'home' | 'profiles' | 'settings' | 'logs' | 'about';
  readonly onChange: (tab: 'home' | 'profiles' | 'settings' | 'logs' | 'about') => void;
}

export function Navigation({ activeTab, onChange }: NavigationProps) {
  const { t } = useTranslation();

  const tabs = [
    { id: 'home', icon: Home, label: t('navHome') },
    { id: 'profiles', icon: List, label: t('navProfiles') },
    { id: 'settings', icon: Settings, label: t('navSettings') },
    { id: 'logs', icon: Terminal, label: t('navLogs') },
    { id: 'about', icon: Info, label: t('about') },
  ] as const;

  return (
    <div className="w-20 border-r border-white/5 flex flex-col items-center py-8 gap-8 bg-zinc-950/80 backdrop-blur-2xl z-20 shadow-[4px_0_24px_rgba(0,0,0,0.2)]">
      <div className="w-11 h-11 bg-gradient-to-br from-emerald-400 to-emerald-600 rounded-xl flex items-center justify-center shadow-[0_0_20px_rgba(16,185,129,0.3)] mb-4">
        <List className="w-6 h-6 text-zinc-950" strokeWidth={2.5} />
      </div>
      <div className="flex flex-col gap-4 w-full items-center">
        {tabs.map(({ id, icon: Icon, label }) => (
          <button
            key={id}
            onClick={() => onChange(id)}
            title={label}
            className={`relative p-3.5 rounded-xl transition-all duration-300 group ${
              activeTab === id ? 'text-emerald-400' : 'text-zinc-500 hover:text-zinc-300'
            }`}
          >
            {activeTab === id && (
              <motion.div
                layoutId="nav-bg"
                className="absolute inset-0 bg-emerald-500/10 rounded-xl border border-emerald-500/20"
                transition={{ type: 'spring', bounce: 0.2, duration: 0.6 }}
              />
            )}
            <Icon size={22} className="relative z-10 group-hover:scale-110 transition-transform duration-300" />
          </button>
        ))}
      </div>
      <div className="mt-auto mb-4">
        <div className="w-2 h-2 rounded-full bg-emerald-500 shadow-[0_0_12px_rgba(16,185,129,0.8)]" />
      </div>
    </div>
  );
}
