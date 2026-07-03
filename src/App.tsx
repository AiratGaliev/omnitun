import { useEffect, useState } from 'react';
import { Toaster } from 'sonner';
import { AnimatePresence, motion } from 'framer-motion';
import { Loader2, Shield } from 'lucide-react';
import { useAppStore } from './store';
import { i18n } from './i18n';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Onboarding } from './components/screens/Onboarding';
import { Dashboard } from './components/screens/Dashboard';
import { Profiles } from './components/screens/Profiles';
import { Settings } from './components/screens/Settings';
import { Logs } from './components/screens/Logs';
import { About } from './components/screens/About';
import { Terms } from './components/screens/Terms';
import { Navigation } from './components/Navigation';
import type { Tab } from '@/types.ts';

const pageVariants = {
  initial: { opacity: 0, y: 15, scale: 0.99 },
  animate: { opacity: 1, y: 0, scale: 1 },
  exit: { opacity: 0, y: -15, scale: 0.99 },
};

const pageTransition = {
  duration: 0.4,
  ease: [0.22, 1, 0.36, 1] as const,
};

export default function App() {
  const [activeTab, setActiveTab] = useState<Tab>('home');
  const [isInitialized, setIsInitialized] = useState(false);
  const hasOnboarded = useAppStore((state) => state.hasOnboarded);
  const settings = useAppStore((state) => state.settings);
  const initListener = useAppStore((state) => state.initListener);
  const loadProfiles = useAppStore((state) => state.loadProfiles);
  const loadSettings = useAppStore((state) => state.loadSettings);
  const fetchRealIp = useAppStore((state) => state.fetchRealIp);
  const addLog = useAppStore((state) => state.addLog);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let isMounted = true;

    initListener()
      .then((fn) => {
        if (isMounted) {
          unlisten = fn;
        } else {
          fn(); // already unmounted — clean up immediately
        }
      })
      .catch((e: unknown) => {
        addLog(
          `initListener failed: ${e instanceof Error ? e.message : String(e)}`,
          'warning',
          'system',
        );
      });

    return () => {
      isMounted = false;
      unlisten?.();
    };
  }, [initListener]);

  // Listen for tray toggle event (system tray "Connect" button)
  useEffect(() => {
    const unlistenPromise = listen('tray-toggle-proxy', () => {
      void useAppStore.getState().toggleConnection();
    });
    return () => {
      unlistenPromise.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    void Promise.all([
      loadProfiles().catch(() => {
        addLog('Failed to load profiles on startup', 'warning', 'subscription');
      }),
      loadSettings().catch(() => {
        addLog('Failed to load settings on startup, using defaults', 'warning', 'settings');
      }),
      invoke('initialize_app').catch(() => {
        addLog(
          'Background initialization failed (offline?) — app will work without GeoIP',
          'warning',
          'system',
        );
      }),
    ]).then(async () => {
      setIsInitialized(true);
      await fetchRealIp().catch(() => {
        addLog('Failed to fetch real IP on startup', 'warning', 'system');
      });
      const { profiles: loadedProfiles, settings: loadedSettings } = useAppStore.getState();
      addLog(
        `App initialized: ${String(loadedProfiles.length)} profile(s), language: ${loadedSettings.language}`,
        'info',
        'system',
      );
    });
  }, [loadProfiles, loadSettings, addLog, fetchRealIp]);

  useEffect(() => {
    void i18n.changeLanguage(settings.language);
  }, [settings.language]);

  useEffect(() => {
    if (!isInitialized || !settings.autoUpdateInterval || settings.autoUpdateInterval <= 0) return;

    const checkAndUpdate = () => {
      const { profiles, refreshProfile, addLog, settings } = useAppStore.getState();
      if (!settings.autoUpdateInterval || settings.autoUpdateInterval <= 0) return;

      const intervalMs = settings.autoUpdateInterval * 60 * 1000;
      const now = Date.now();

      let updatedCount = 0;
      profiles.forEach((p) => {
        const lastUpdated = p.lastUpdated ?? 0;
        if (now - lastUpdated >= intervalMs) {
          updatedCount++;
          refreshProfile(p.id).catch(() => {
            addLog(`Auto-refresh failed for profile ${p.id}`, 'warning', 'subscription');
          });
        }
      });

      if (updatedCount > 0) {
        addLog(`Auto-refreshing ${String(updatedCount)} subscription(s)`, 'info', 'subscription');
      }
    };

    const initialTimer = setTimeout(checkAndUpdate, 3000);
    const timer = setInterval(checkAndUpdate, 60 * 1000);

    return () => {
      clearTimeout(initialTimer);
      clearInterval(timer);
    };
  }, [isInitialized, settings.autoUpdateInterval]);

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

      {!isInitialized ? (
        <div className="w-full h-screen bg-zinc-950 flex items-center justify-center">
          <div className="flex flex-col items-center gap-4">
            <Shield className="w-10 h-10 text-emerald-400" />
            <Loader2 className="w-6 h-6 text-zinc-500 animate-spin" />
            <p className="text-zinc-500 text-sm">Downloading resources...</p>
          </div>
        </div>
      ) : (
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
              <div className="flex-1 relative overflow-hidden from-zinc-950">
                <AnimatePresence mode="wait">
                  {activeTab === 'home' && (
                    <motion.div
                      key="home"
                      variants={pageVariants}
                      initial="initial"
                      animate="animate"
                      exit="exit"
                      transition={pageTransition}
                      className="absolute inset-0"
                    >
                      <Dashboard onNavigate={setActiveTab} />
                    </motion.div>
                  )}
                  {activeTab === 'profiles' && (
                    <motion.div
                      key="profiles"
                      variants={pageVariants}
                      initial="initial"
                      animate="animate"
                      exit="exit"
                      transition={pageTransition}
                      className="absolute inset-0"
                    >
                      <Profiles />
                    </motion.div>
                  )}
                  {activeTab === 'settings' && (
                    <motion.div
                      key="settings"
                      variants={pageVariants}
                      initial="initial"
                      animate="animate"
                      exit="exit"
                      transition={pageTransition}
                      className="absolute inset-0"
                    >
                      <Settings />
                    </motion.div>
                  )}
                  {activeTab === 'logs' && (
                    <motion.div
                      key="logs"
                      variants={pageVariants}
                      initial="initial"
                      animate="animate"
                      exit="exit"
                      transition={pageTransition}
                      className="absolute inset-0"
                    >
                      <Logs />
                    </motion.div>
                  )}
                  {activeTab === 'about' && (
                    <motion.div
                      key="about"
                      variants={pageVariants}
                      initial="initial"
                      animate="animate"
                      exit="exit"
                      transition={pageTransition}
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
      )}
    </div>
  );
}
