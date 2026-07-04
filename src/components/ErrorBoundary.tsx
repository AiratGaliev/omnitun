import { Component, type ErrorInfo, type ReactNode } from 'react';
import { ShieldAlert } from 'lucide-react';
import { useAppStore } from '@/store.ts';

interface Props {
  children: ReactNode;
}

interface State {
  hasError: boolean;
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  public state: State = {
    hasError: false,
    error: null,
  };

  public static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error };
  }

  public componentDidCatch(error: Error, _errorInfo: ErrorInfo) {
    // Use store directly — safe even during render crashes since getState()
    // does not trigger React reconciliation.
    useAppStore.getState().addLog(`[UI] Render error: ${error.message}`, 'warning', 'system');
  }

  public render() {
    if (this.state.hasError) {
      return (
        <div className="flex flex-col items-center justify-center w-full h-screen bg-zinc-950 text-zinc-200 p-8 text-center">
          <ShieldAlert size={64} className="text-red-500 mb-6" />
          <h1 className="text-2xl font-bold mb-2">UI Crashed Unexpectedly</h1>
          <p className="text-zinc-400 mb-6 max-w-md">
            A critical rendering error occurred. The backend proxy is likely still running.
          </p>
          <div className="bg-zinc-900 p-4 rounded-xl text-left text-xs font-mono text-red-400 max-w-2xl overflow-auto mb-8">
            {this.state.error?.message}
          </div>
          <button
            onClick={() => {
              globalThis.location.reload();
            }}
            className="bg-emerald-500 text-zinc-950 px-6 py-3 rounded-xl font-bold hover:bg-emerald-400 transition-colors"
          >
            Reload Application
          </button>
        </div>
      );
    }

    return this.props.children;
  }
}
