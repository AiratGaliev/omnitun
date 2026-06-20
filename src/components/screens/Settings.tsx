import { AnimatePresence, motion } from 'framer-motion';
import { Check, Globe, Loader2, Lock, Network, Rocket, Timer } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from '@/hooks/useTranslation.ts';
import { useVPNStore } from '@/store.ts';
import { SegmentedControl } from '../SegmentedControl';
import { GeneralSettings } from '../settings/GeneralSettings';
import { NetworkSettings } from '../settings/NetworkSettings';
import { FilteringSettings } from '../settings/FilteringSettings';
import { ProtocolSettings } from '../settings/ProtocolSettings';

export function Settings() {
  const [activeTab, setActiveTab] = useState<'general' | 'network' | 'filtering' | 'protocols'>(
    'general',
  );
  const { t } = useTranslation();
  const settingsVersion = useVPNStore((state) => state.settingsVersion);
  const prevVersionRef = useRef(settingsVersion);
  const [saveState, setSaveState] = useState<'idle' | 'countdown' | 'saving'>('idle');
  const [countdown, setCountdown] = useState(5);

  useEffect(() => {
    if (prevVersionRef.current === settingsVersion) return;
    prevVersionRef.current = settingsVersion;
    setSaveState('countdown');
    setCountdown(5);
  }, [settingsVersion]);

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout>;
    if (saveState === 'countdown') {
      if (countdown > 0) {
        timer = setTimeout(() => {
          setCountdown((c) => c - 1);
        }, 1000);
      } else {
        timer = setTimeout(() => {
          setSaveState('saving');
          void useVPNStore.getState().saveSettings();
        }, 0);
      }
    } else if (saveState === 'saving') {
      timer = setTimeout(() => {
        setSaveState('idle');
      }, 500);
    }
    return () => {
      clearTimeout(timer);
    };
  }, [saveState, countdown]);

  const handleManualSave = () => {
    if (saveState === 'countdown') {
      setSaveState('saving');
      void useVPNStore.getState().saveSettings();
    }
  };

  return (
    <div className="flex flex-col h-full w-full p-10 pt-8">
      <header className="mb-8 flex justify-between items-start gap-4">
        <div>
          <h1 className="text-3xl font-bold tracking-tight text-white">{t('systemConfig')}</h1>
          <p className="text-zinc-400 text-sm mt-1.5">{t('systemConfigDesc')}</p>
        </div>

        <button
          onClick={handleManualSave}
          disabled={saveState !== 'countdown'}
          className={`shrink-0 flex items-center justify-center gap-2.5 px-4 py-2.5 rounded-xl text-sm font-semibold transition-all duration-300 border ${
            saveState === 'countdown'
              ? 'bg-emerald-500/15 text-emerald-400 border-emerald-500/30 hover:bg-emerald-500/25 cursor-pointer shadow-[0_0_20px_rgba(16,185,129,0.15)]'
              : 'bg-zinc-900/50 text-zinc-400 border-white/5 cursor-default'
          }`}
        >
          {saveState === 'saving' ? (
            <Loader2 size={18} className="animate-spin text-zinc-400" />
          ) : saveState === 'countdown' ? (
            <Timer size={18} className="animate-pulse" />
          ) : (
            <Check size={18} className="text-emerald-500" />
          )}
          <span className="hidden sm:inline">
            {saveState === 'saving'
              ? t('saving')
              : saveState === 'countdown'
                ? `${t('savingIn')} ${String(countdown)}s`
                : t('saved')}
          </span>
        </button>
      </header>

      <div className="mb-8 max-w-3xl">
        <SegmentedControl
          options={[
            { id: 'general', label: t('general'), icon: <Rocket size={18} /> },
            { id: 'network', label: t('network'), icon: <Network size={18} /> },
            { id: 'filtering', label: t('filtering'), icon: <Globe size={18} /> },
            { id: 'protocols', label: t('protocols'), icon: <Lock size={18} /> },
          ]}
          value={activeTab}
          onChange={(val) => {
            setActiveTab(val);
          }}
          layoutId="settingsTabIndicator"
        />
      </div>

      <div className="flex-1 overflow-y-auto overflow-x-hidden custom-scrollbar pb-8 max-w-4xl">
        <AnimatePresence mode="wait" initial={false}>
          <motion.div
            key={activeTab}
            initial={{ opacity: 0, y: 10 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -10 }}
            transition={{ duration: 0.2, ease: 'easeOut' }}
            className="space-y-4"
          >
            {activeTab === 'general' && <GeneralSettings />}
            {activeTab === 'network' && <NetworkSettings />}
            {activeTab === 'filtering' && <FilteringSettings />}
            {activeTab === 'protocols' && <ProtocolSettings />}
          </motion.div>
        </AnimatePresence>
      </div>
    </div>
  );
}
