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
    <div className="flex flex-col h-full w-full p-8 pt-6">
      <header className="mb-8 flex justify-between items-start gap-4">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">{t('systemConfig')}</h1>
          <p className="text-zinc-500 text-sm mt-1">{t('systemConfigDesc')}</p>
        </div>

        <button
          onClick={handleManualSave}
          disabled={saveState !== 'countdown'}
          className={`shrink-0 flex items-center justify-center gap-2 px-3 py-1.5 md:px-4 md:py-2 rounded-xl text-xs md:text-sm font-medium transition-colors border ${
            saveState === 'countdown'
              ? 'bg-blue-500/10 text-blue-400 border-blue-500/20 hover:bg-blue-500/20 cursor-pointer shadow-[0_0_15px_rgba(59,130,246,0.15)]'
              : 'bg-black/20 text-zinc-300 border-white/5 cursor-default'
          }`}
        >
          {saveState === 'saving' ? (
            <Loader2 size={16} className="animate-spin text-zinc-400" />
          ) : saveState === 'countdown' ? (
            <Timer size={16} className="animate-pulse" />
          ) : (
            <Check size={16} className="text-emerald-500" />
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

      <div className="mb-8">
        <SegmentedControl
          options={[
            { id: 'general', label: t('general'), icon: <Rocket size={16} /> },
            { id: 'network', label: t('network'), icon: <Network size={16} /> },
            { id: 'filtering', label: t('filtering'), icon: <Globe size={16} /> },
            { id: 'protocols', label: t('protocols'), icon: <Lock size={16} /> },
          ]}
          value={activeTab}
          onChange={(val) => {
            setActiveTab(val);
          }}
          layoutId="settingsTabIndicator"
        />
      </div>

      <div className="flex-1 overflow-y-auto overflow-x-hidden custom-scrollbar pb-4">
        <AnimatePresence mode="wait" initial={false}>
          <motion.div
            key={activeTab}
            initial={{ opacity: 0, x: -10 }}
            animate={{ opacity: 1, x: 0 }}
            exit={{ opacity: 0, x: 10 }}
            transition={{ duration: 0.15 }}
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
