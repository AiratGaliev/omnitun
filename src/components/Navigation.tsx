import { useState } from 'react';
import {
  Globe,
  Home,
  Info,
  List,
  PanelLeftClose,
  PanelLeftOpen,
  Settings,
  ShieldCheck,
  Terminal,
} from 'lucide-react';
import { AnimatePresence, motion } from 'framer-motion';
import { useTranslation } from '../hooks/useTranslation';
import { useAppStore } from '../store';

interface NavigationProps {
  readonly activeTab: 'home' | 'profiles' | 'settings' | 'logs' | 'about';
  readonly onChange: (tab: 'home' | 'profiles' | 'settings' | 'logs' | 'about') => void;
}

export function Navigation({ activeTab, onChange }: NavigationProps) {
  const { t } = useTranslation();
  const [isExpanded, setIsExpanded] = useState(false);
  const isConnected = useAppStore((state) => state.status === 'connected');

  const tabs = [
    { id: 'home', icon: Home, label: t('navHome') },
    { id: 'profiles', icon: List, label: t('navProfiles') },
    { id: 'settings', icon: Settings, label: t('navSettings') },
    { id: 'logs', icon: Terminal, label: t('navLogs') },
    { id: 'about', icon: Info, label: t('about') },
  ] as const;

  return (
    <motion.div
      animate={{ width: isExpanded ? 240 : 88 }}
      transition={{ type: 'spring', bounce: 0, duration: 0.4 }}
      className="border-r border-white/5 flex flex-col py-6 bg-zinc-950/80 backdrop-blur-2xl z-20 shadow-[4px_0_24px_rgba(0,0,0,0.2)] overflow-hidden shrink-0"
    >
      <div
        className={`flex items-center h-12 w-full mb-8 ${isExpanded ? 'px-6 justify-between' : 'justify-center'}`}
      >
        <AnimatePresence>
          {isExpanded && (
            <motion.div
              initial={{ opacity: 0, width: 0 }}
              animate={{ opacity: 1, width: 'auto' }}
              exit={{ opacity: 0, width: 0 }}
              className="flex items-center gap-3 overflow-hidden whitespace-nowrap"
            >
              <div className="w-9 h-9 bg-emerald-500 rounded-xl flex items-center justify-center shadow-lg shrink-0">
                <ShieldCheck size={20} className="text-zinc-950" strokeWidth={2.5} />
              </div>
              <span className="font-bold text-lg tracking-wide text-white">OmniTun</span>
            </motion.div>
          )}
        </AnimatePresence>

        <button
          onClick={() => {
            setIsExpanded(!isExpanded);
          }}
          className="p-2.5 rounded-xl hover:bg-white/5 text-zinc-400 hover:text-white transition-colors shrink-0"
          title={isExpanded ? 'Свернуть панель' : 'Развернуть панель'}
        >
          {isExpanded ? <PanelLeftClose size={22} /> : <PanelLeftOpen size={22} />}
        </button>
      </div>

      <div className="flex flex-col gap-3 w-full px-4">
        {tabs.map(({ id, icon: Icon, label }) => (
          <button
            key={id}
            onClick={() => {
              onChange(id);
            }}
            title={!isExpanded ? label : undefined}
            className={`relative flex items-center ${
              isExpanded ? 'justify-start px-4' : 'justify-center'
            } py-3.5 rounded-xl transition-all duration-300 group ${
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

            <Icon
              size={22}
              className="relative z-10 group-hover:scale-110 transition-transform duration-300 shrink-0"
            />

            <AnimatePresence>
              {isExpanded && (
                <motion.span
                  initial={{ opacity: 0, width: 0, marginLeft: 0 }}
                  animate={{ opacity: 1, width: 'auto', marginLeft: 16 }}
                  exit={{ opacity: 0, width: 0, marginLeft: 0 }}
                  className="relative z-10 font-medium text-sm whitespace-nowrap overflow-hidden"
                >
                  {label}
                </motion.span>
              )}
            </AnimatePresence>
          </button>
        ))}
      </div>

      <div
        className={`mt-auto mb-2 flex items-center h-8 ${isExpanded ? 'px-8 justify-start gap-4' : 'justify-center w-full'}`}
      >
        <Globe
          size={14}
          className={`shrink-0 ${isConnected ? 'text-emerald-500 animate-pulse' : 'text-zinc-600'}`}
          style={isConnected ? { filter: 'drop-shadow(0 0 8px rgba(16,185,129,0.6))' } : undefined}
        />
        <AnimatePresence>
          {isExpanded && (
            <motion.span
              initial={{ opacity: 0, width: 0 }}
              animate={{ opacity: 1, width: 'auto' }}
              exit={{ opacity: 0, width: 0 }}
              className="text-[11px] font-bold text-zinc-500 uppercase tracking-widest whitespace-nowrap overflow-hidden"
            >
              System Active
            </motion.span>
          )}
        </AnimatePresence>
      </div>
    </motion.div>
  );
}
