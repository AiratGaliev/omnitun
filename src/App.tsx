import { useEffect, useRef, useState } from 'react';
import { Toaster, toast } from 'sonner';
import { AnimatePresence, motion } from 'framer-motion';
import { Loader2, Shield } from 'lucide-react';
import { useTranslation } from './hooks/useTranslation';
import { useAppStore } from './store';
import { i18n } from './i18n';
import { getErrorMessage } from './lib/errors';
import { isRustError } from './types';
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
  const { t } = useTranslation();

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
        addLog(`initListener failed: ${getErrorMessage(e)}`, 'warning', 'system');
      });

    return () => {
      isMounted = false;
      unlisten?.();
    };
  }, [initListener, addLog]);

  // Listen for tray toggle event (system tray "Connect" button)
  useEffect(() => {
    const unlistenPromise = listen('tray-toggle-proxy', () => {
      void useAppStore.getState().toggleConnection();
    });
    return () => {
      unlistenPromise
        .then((fn) => {
          fn();
        })
        .catch(() => {
          useAppStore.getState().addLog('Failed to clean up tray listener', 'warning', 'system');
        });
    };
  }, []);

  // Guard: ensure initialize_app is invoked only once per component lifecycle,
  // even under React StrictMode (which double-invokes effects in dev) or if
  // the effect dependencies change.
  const initRef = useRef(false);

  useEffect(() => {
    if (initRef.current) return;
    initRef.current = true;

    // initialize_app fires independently — if it fails, the app still
    // loads profiles/settings and shows the UI (limited GeoIP without DB).
    // Backend logs initialize_app errors via tracing::error! — no frontend
    // duplicate needed.
    void invoke('initialize_app');

    void Promise.all([loadProfiles(), loadSettings()]).then(async () => {
      setIsInitialized(true);
      await fetchRealIp();
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

      // ── Stagger refreshes (UI freeze fix) ──────────────────────────────
      // Previously, ALL due profiles were refreshed simultaneously. Each
      // refresh triggers process_servers (TCP ping + batch test) which
      // emits a burst of refresh-progress events. With N profiles refreshing
      // at once, the events from all profiles interleave into a massive
      // burst that saturates the frontend.
      //
      // We now stagger refresh starts by ~400ms per profile.
      const due = profiles.filter((p) => {
        const lastUpdated = p.lastUpdated ?? 0;
        return now - lastUpdated >= intervalMs;
      });

      if (due.length > 0) {
        addLog(`Auto-refreshing ${String(due.length)} subscription(s)`, 'info', 'subscription');
      }

      const STAGGER_MS = 400;
      due.forEach((p, index) => {
        setTimeout(() => {
          // Re-check that the profile still exists and is still due
          const current = useAppStore.getState().profiles.find((x) => x.id === p.id);
          if (!current) return;
          const currentInterval = useAppStore.getState().settings.autoUpdateInterval;
          if (!currentInterval || currentInterval <= 0) return;
          const currentIntervalMs = currentInterval * 60 * 1000;
          if (Date.now() - (current.lastUpdated ?? 0) < currentIntervalMs) return;

          refreshProfile(p.id).catch((err: unknown) => {
            if (isRustError(err)) {
              toast.error(t(err.code, { defaultValue: err.message }));
            } else {
              toast.error(t('autoRefreshFailed'));
            }
          });
        }, index * STAGGER_MS);
      });
    };

    const initialTimer = setTimeout(checkAndUpdate, 3000);
    const timer = setInterval(checkAndUpdate, 60 * 1000);

    return () => {
      clearTimeout(initialTimer);
      clearInterval(timer);
    };
  }, [isInitialized, settings.autoUpdateInterval, t]);

  // Separate server TESTING timer ───────────────────────────────
  // This is distinct from subscription REFRESH (above). Testing re-runs
  // batch-test on EXISTING servers (no URL fetch) to keep latency/exit-IP
  // fresh. Cadence: `autoTestInterval` (default 10 min). 0 = disabled.
  useEffect(() => {
    if (!isInitialized || !settings.autoTestInterval || settings.autoTestInterval <= 0) return;

    const checkAndTest = () => {
      const { profiles, testServers, addLog, settings } = useAppStore.getState();
      if (!settings.autoTestInterval || settings.autoTestInterval <= 0) return;

      const testIntervalMs = settings.autoTestInterval * 60 * 1000;
      const now = Date.now();

      const due = profiles.filter((p) => {
        const lastTested = p.lastTested ?? p.lastUpdated ?? 0;
        return now - lastTested >= testIntervalMs;
      });

      if (due.length > 0) {
        addLog(`Auto-testing ${String(due.length)} profile(s) (no URL fetch)`, 'info', 'cache');
      }

      const STAGGER_MS = 400;
      due.forEach((p, index) => {
        setTimeout(() => {
          const current = useAppStore.getState().profiles.find((x) => x.id === p.id);
          if (!current) return;
          const currentInterval = useAppStore.getState().settings.autoTestInterval;
          if (!currentInterval || currentInterval <= 0) return;
          const currentIntervalMs = currentInterval * 60 * 1000;
          const lastTested = current.lastTested ?? current.lastUpdated ?? 0;
          if (Date.now() - lastTested < currentIntervalMs) return;
          if (useAppStore.getState()._refreshingIds.includes(p.id)) return;
          void testServers(p.id);
        }, index * STAGGER_MS);
      });
    };

    const initialTestTimer = setTimeout(checkAndTest, 10000);
    const testTimer = setInterval(checkAndTest, 60 * 1000);

    return () => {
      clearTimeout(initialTestTimer);
      clearInterval(testTimer);
    };
  }, [isInitialized, settings.autoTestInterval]);

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
