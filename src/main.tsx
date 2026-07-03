import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import App from './App.tsx';
import './index.css';
import './i18n';
import { ErrorBoundary } from './components/ErrorBoundary';
import { useAppStore } from './store';

// Global handlers for uncaught JS errors and promise rejections.
// These are LAST-RESORT safety nets — by the time they fire, the
// error has bypassed all try/catch, .catch(), ErrorBoundary, and
// backend error handling. Add them to frontend log state so the
// user can see them in the Logs viewer.
globalThis.addEventListener('unhandledrejection', (event) => {
  const msg = event.reason instanceof Error ? event.reason.message : String(event.reason);
  useAppStore.getState().addLog(`[UI] Unhandled Promise Rejection: ${msg}`, 'warning', 'system');
});

globalThis.addEventListener('error', (event) => {
  useAppStore.getState().addLog(`[UI] Uncaught Error: ${event.message}`, 'warning', 'system');
});

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>,
);
