import { useEffect, useState } from 'react';
import { Toaster } from 'sonner';
import { AnimatePresence, motion } from 'framer-motion';
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

const pageVariants = {
  initial: { opacity: 0, y: 15, scale: 0.99 },
  animate: { opacity: 1, y: 0, scale: 1 },
  exit: { opacity: 0, y: -15, scale: 0.99 },
};

const pageTransition = {
  duration: 0.4,
  ease: [0.22, 1, 0.36, 1],
};

export default function App() {
  const [activeTab, setActiveTab] = useState<Tab>('home');
  const [isInitialized, setIsInitialized] = useState(false);
  const hasOnboarded = useVPNStore((state) => state.hasOnboarded);
  const { profiles, settings } = useVPNStore();
  const initListener = useVPNStore((state) => state.initListener);
  const loadProfiles = useVPNStore((state) => state.loadProfiles);
  const loadSettings = useVPNStore((state) => state.loadSettings);
  const refreshProfile = useVPNStore((state) => state.refreshProfile);
  const addLog = useVPNStore((state) => state.addLog);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    initListener()
      .then((fn) => { unlisten = fn; })
      .catch(() => undefined);
    return () => { unlisten?.(); };
  }, [initListener]);

  useEffect(() => {
    void Promise.all([
      loadProfiles().catch(() => undefined),
      loadSettings().catch(() => undefined),
    ]).then(() => {
      setIsInitialized(true);
      const { profiles: loadedProfiles, settings: loadedSettings } = useVPNStore.getState();
      addLog(
        `App initialized: ${String(loadedProfiles.length)} profile(s), language: ${loadedSettings.language}`,
        'info',
        'system',
      );
    });
  }, [loadProfiles, loadSettings, addLog]);

  useEffect(() => {
    void i18n.changeLanguage(settings.language);
  }, [settings.language]);

  useEffect(() => {
    if (!settings.autoUpdateInterval || settings.autoUpdateInterval <= 0) return;
    const intervalMs = settings.autoUpdateInterval * 60 * 1000;
    const timer = setInterval(() => {
      addLog(`Auto-refreshing ${String(profiles.length)} subscription(s)`, 'info', 'subscription');
      profiles.forEach((p) => {
        refreshProfile(p.id).catch(() => undefined);
      });
    }, intervalMs);
    return () => { clearInterval(timer); };
  }, [profiles, settings.autoUpdateInterval, refreshProfile, addLog]);

  return (
    <div className="w-full h-screen bg-zinc-950 text-zinc-100 font-sans flex overflow-hidden relative selection:bg-emerald-500/30">
      <Toaster
        theme="dark"
        position="bottom-center"
        toastOptions={{
          style: {
            background: 'rgba(24, 24, 27, 0.8)',
            backdropFilter: 'blur(12px)',
            border: '1px solid rgba(255,255,255,0.05)',
            color: '#f4f4f5',
            borderRadius: '1rem',
          },
        }}
      />

      {!isInitialized ? null : (
        <AnimatePresence mode="wait">
          {!settings.acceptTerms ? (
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
              <div className="flex-1 relative overflow-hidden bg-gradient-to-br from-zinc-950 to-zinc-900/50">
                <AnimatePresence mode="wait">
                  {activeTab === 'home' && (
                    <motion.div key="home" variants={pageVariants} initial="initial" animate="animate" exit="exit" transition={pageTransition} className="absolute inset-0">
                      <Dashboard onNavigate={setActiveTab} />
                    </motion.div>
                  )}
                  {activeTab === 'profiles' && (
                    <motion.div key="profiles" variants={pageVariants} initial="initial" animate="animate" exit="exit" transition={pageTransition} className="absolute inset-0">
                      <Profiles />
                    </motion.div>
                  )}
                  {activeTab === 'settings' && (
                    <motion.div key="settings" variants={pageVariants} initial="initial" animate="animate" exit="exit" transition={pageTransition} className="absolute inset-0">
                      <Settings />
                    </motion.div>
                  )}
                  {activeTab === 'logs' && (
                    <motion.div key="logs" variants={pageVariants} initial="initial" animate="animate" exit="exit" transition={pageTransition} className="absolute inset-0">
                      <Logs />
                    </motion.div>
                  )}
                  {activeTab === 'about' && (
                    <motion.div key="about" variants={pageVariants} initial="initial" animate="animate" exit="exit" transition={pageTransition} className="absolute inset-0">
                      <About />
                    </motion.div>
                  )}
                </AnimatePresence>
              </div>
            </motion.div>
          )}
        </AnimatePresence>
      )}
    </div>
  );
}
