import { Home, List, Settings, Info, Terminal } from 'lucide-react';
import { motion } from 'framer-motion';
import { useTranslation } from '../hooks/useTranslation';

interface NavigationProps {
  activeTab: 'home' | 'profiles' | 'settings' | 'logs' | 'about';
  onChange: (tab: 'home' | 'profiles' | 'settings' | 'logs' | 'about') => void;
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
    <div className="w-20 border-r border-white/5 flex flex-col items-center py-8 gap-8 bg-zinc-950/50 backdrop-blur-xl z-20">
      <div className="w-10 h-10 bg-emerald-500 rounded-xl flex items-center justify-center shadow-lg shadow-emerald-500/20 mb-4">
        <List className="w-6 h-6 text-zinc-950" />
      </div>
      <div className="flex flex-col gap-6 w-full items-center">
        {tabs.map(({ id, icon: Icon, label }) => (
          <button
            key={id}
            onClick={() => {
              onChange(id);
            }}
            title={label}
            className={`relative p-3 rounded-xl transition-colors ${
              activeTab === id ? 'text-emerald-500 bg-white/5' : 'text-zinc-500 hover:text-zinc-200'
            }`}
          >
            {activeTab === id && (
              <motion.div
                layoutId="nav-bg"
                className="absolute inset-0 bg-white/5 rounded-xl border border-white/10"
                transition={{ type: 'spring', bounce: 0.2, duration: 0.6 }}
              />
            )}
            <Icon size={24} className="relative z-10" />
          </button>
        ))}
      </div>
      <div className="mt-auto mb-4">
        <div className="w-2 h-2 rounded-full bg-emerald-500 animate-pulse shadow-[0_0_10px_rgba(16,185,129,0.5)]"></div>
      </div>
    </div>
  );
}
