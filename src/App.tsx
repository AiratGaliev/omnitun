import { useState, useEffect } from 'react';
import { Toaster } from 'sonner';
import { motion, AnimatePresence } from 'framer-motion';
import { useVPNStore } from './store';
import { i18n } from './i18n';
import { Onboarding } from './components/screens/Onboarding';
import { Dashboard } from './components/screens/Dashboard';
import { Profiles } from './components/screens/Profiles';
import { Settings } from './components/screens/Settings';
import { Logs } from './components/screens/Logs';
import { About } from './components/screens/About';
import { Terms } from './components/screens/Terms';
import { Navigation } from './components/Navigation';

type Tab = 'home' | 'profiles' | 'settings' | 'logs' | 'about';

export default function App() {
  const [activeTab, setActiveTab] = useState<Tab>('home');
  const hasOnboarded = useVPNStore((state) => state.hasOnboarded);
  const hasAcceptedTerms = useVPNStore((state) => state.hasAcceptedTerms);
  const { profiles, settings } = useVPNStore();
  const initListener = useVPNStore((state) => state.initListener);
  const loadProfiles = useVPNStore((state) => state.loadProfiles);
  const loadSettings = useVPNStore((state) => state.loadSettings);
  const refreshProfile = useVPNStore((state) => state.refreshProfile);

  // Init Tauri event listener on mount
  useEffect(() => {
    let unlisten: (() => void) | undefined;

    initListener()
      .then((fn) => {
        unlisten = fn;
      })
      .catch(() => undefined);

    return () => {
      unlisten?.();
    };
  }, [initListener]);

  // Load saved profiles and settings on mount
  useEffect(() => {
    loadProfiles().catch(() => undefined);
    loadSettings().catch(() => undefined);
  }, [loadProfiles, loadSettings]);

  // Apply persisted language to i18n when settings load
  useEffect(() => {
    void i18n.changeLanguage(settings.language);
  }, [settings.language]);

  // Auto-refresh subscriptions at interval
  useEffect(() => {
    if (!settings.autoUpdateInterval || settings.autoUpdateInterval <= 0) return;

    const intervalMs = settings.autoUpdateInterval * 60 * 1000;
    const timer = setInterval(() => {
      profiles.forEach((p) => {
        refreshProfile(p.id).catch(() => undefined);
      });
    }, intervalMs);

    return () => {
      clearInterval(timer);
    };
  }, [profiles, settings.autoUpdateInterval, refreshProfile]);

  return (
    <div className="w-full h-screen bg-zinc-950 text-zinc-100 font-sans flex overflow-hidden relative">
      <Toaster
        theme="dark"
        position="bottom-center"
        toastOptions={{
          style: {
            background: '#18181b',
            border: '1px solid rgba(255,255,255,0.1)',
            color: '#f4f4f5',
          },
        }}
      />

      <AnimatePresence mode="wait">
        {!hasAcceptedTerms ? (
          <Terms key="terms" />
        ) : !hasOnboarded ? (
          <Onboarding key="onboarding" />
        ) : (
          <motion.div
            key="main"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            className="flex-1 flex flex-row w-full h-full"
          >
            <Navigation activeTab={activeTab} onChange={setActiveTab} />
            <div className="flex-1 relative overflow-hidden">
              <AnimatePresence mode="wait">
                {activeTab === 'home' && (
                  <motion.div
                    key="home"
                    initial={{ opacity: 0, x: -20 }}
                    animate={{ opacity: 1, x: 0 }}
                    exit={{ opacity: 0, x: 20 }}
                    transition={{ duration: 0.2 }}
                    className="absolute inset-0"
                  >
                    <Dashboard />
                  </motion.div>
                )}
                {activeTab === 'profiles' && (
                  <motion.div
                    key="profiles"
                    initial={{ opacity: 0, x: 20 }}
                    animate={{ opacity: 1, x: 0 }}
                    exit={{ opacity: 0, x: -20 }}
                    transition={{ duration: 0.2 }}
                    className="absolute inset-0"
                  >
                    <Profiles />
                  </motion.div>
                )}
                {activeTab === 'settings' && (
                  <motion.div
                    key="settings"
                    initial={{ opacity: 0, x: 20 }}
                    animate={{ opacity: 1, x: 0 }}
                    exit={{ opacity: 0, x: -20 }}
                    transition={{ duration: 0.2 }}
                    className="absolute inset-0"
                  >
                    <Settings />
                  </motion.div>
                )}
                {activeTab === 'logs' && (
                  <motion.div
                    key="logs"
                    initial={{ opacity: 0, x: 20 }}
                    animate={{ opacity: 1, x: 0 }}
                    exit={{ opacity: 0, x: -20 }}
                    transition={{ duration: 0.2 }}
                    className="absolute inset-0"
                  >
                    <Logs />
                  </motion.div>
                )}
                {activeTab === 'about' && (
                  <motion.div
                    key="about"
                    initial={{ opacity: 0, x: 20 }}
                    animate={{ opacity: 1, x: 0 }}
                    exit={{ opacity: 0, x: -20 }}
                    transition={{ duration: 0.2 }}
                    className="absolute inset-0"
                  >
                    <About />
                  </motion.div>
                )}
              </AnimatePresence>
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
