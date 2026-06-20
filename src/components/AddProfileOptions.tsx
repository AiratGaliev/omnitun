import { useState } from 'react';
import { AnimatePresence, motion } from 'framer-motion';
import { Clipboard, Plus, X } from 'lucide-react';
import { toast } from 'sonner';
import { useTranslation } from '@/hooks/useTranslation';
import * as z from 'zod';

const manualSchema = z.object({
  name: z.string().min(1, 'Name is required'),
  // eslint-disable-next-line @typescript-eslint/no-deprecated
  url: z.string().url('Invalid URL format'),
});

interface AddProfileOptionsProps {
  readonly onAdd: (url: string, name?: string) => void;
  readonly className?: string;
  readonly horizontal?: boolean;
}

export function AddProfileOptions({
  onAdd,
  className = '',
  horizontal = false,
}: AddProfileOptionsProps) {
  const { t } = useTranslation();
  const [viewMode, setViewMode] = useState<'initial' | 'manual'>('initial');
  const [name, setName] = useState('');
  const [url, setUrl] = useState('');

  const handleClipboardAdd = async () => {
    try {
      const clipboardText = await navigator.clipboard.readText();
      if (
        clipboardText.startsWith('vless://') ||
        clipboardText.startsWith('ss://') ||
        clipboardText.startsWith('vmess://') ||
        clipboardText.startsWith('trojan://') ||
        clipboardText.startsWith('http')
      ) {
        onAdd(clipboardText);
      } else {
        throw new Error('invalid config');
      }
    } catch {
      toast.error(t('clipboardErrMsg'), { description: t('clipboardErrDesc'), duration: 4000 });
    }
  };

  const handleManualAdd = () => {
    const result = manualSchema.safeParse({ name, url });
    if (!result.success) {
      toast.error(result.error.issues[0].message);
      return;
    }
    onAdd(url, name);
    setViewMode('initial');
    setName('');
    setUrl('');
  };

  return (
    <div className={`w-full max-w-md ${className}`}>
      <div
        className={`flex items-center justify-center gap-5 ${horizontal ? 'flex-row' : 'flex-col sm:flex-row'}`}
      >
        <button
          onClick={() => void handleClipboardAdd()}
          className="flex-1 aspect-4/3 max-h-32 bg-zinc-900/40 backdrop-blur-md border border-white/5 hover:border-emerald-500/30 hover:bg-zinc-800/60 rounded-3xl flex flex-col items-center justify-center gap-4 transition-all duration-300 group shadow-lg"
        >
          <div className="p-3 bg-zinc-800/80 rounded-2xl group-hover:bg-emerald-500/10 transition-colors">
            <Clipboard
              size={28}
              strokeWidth={1.5}
              className="text-zinc-400 group-hover:text-emerald-400 transition-colors"
            />
          </div>
          <span className="text-sm font-semibold text-zinc-300 group-hover:text-white transition-colors">
            {t('fromClipboard')}
          </span>
        </button>

        <button
          onClick={() => {
            setViewMode('manual');
          }}
          className="flex-1 aspect-4/3 max-h-32 bg-zinc-900/40 backdrop-blur-md border border-white/5 hover:border-emerald-500/30 hover:bg-zinc-800/60 rounded-3xl flex flex-col items-center justify-center gap-4 transition-all duration-300 group shadow-lg"
        >
          <div className="p-3 bg-zinc-800/80 rounded-2xl group-hover:bg-emerald-500/10 transition-colors">
            <Plus
              size={28}
              strokeWidth={1.5}
              className="text-zinc-400 group-hover:text-emerald-400 transition-colors"
            />
          </div>
          <span className="text-sm font-semibold text-zinc-300 group-hover:text-white transition-colors">
            {t('manualAdd')}
          </span>
        </button>
      </div>

      <AnimatePresence>
        {viewMode === 'manual' && (
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            className="fixed inset-0 z-50 flex items-center justify-center p-8 bg-zinc-950/80 backdrop-blur-sm"
          >
            <motion.div
              initial={{ scale: 0.95, y: 20 }}
              animate={{ scale: 1, y: 0 }}
              exit={{ scale: 0.95, y: 20 }}
              transition={{ type: 'spring', duration: 0.5, bounce: 0.3 }}
              className="w-full max-w-md bg-zinc-900/95 backdrop-blur-2xl border border-white/10 rounded-3xl shadow-2xl overflow-hidden flex flex-col text-left relative z-50"
            >
              <div className="flex justify-between items-center px-8 py-6 border-b border-white/5">
                <h2 className="text-xl font-bold tracking-tight text-white">{t('manualAdd')}</h2>
                <button
                  onClick={() => {
                    setViewMode('initial');
                  }}
                  className="p-2 bg-white/5 hover:bg-white/10 rounded-full text-zinc-400 hover:text-white transition-colors"
                >
                  <X size={18} strokeWidth={2.5} />
                </button>
              </div>

              <div className="p-8 flex flex-col gap-6">
                <div className="flex flex-col gap-2">
                  <label className="text-xs font-semibold text-zinc-400 uppercase tracking-wider pl-1">
                    {t('nameField')}
                  </label>
                  <input
                    type="text"
                    value={name}
                    onChange={(e) => {
                      setName(e.target.value);
                    }}
                    className="w-full bg-zinc-950/50 border border-white/10 rounded-xl px-4 py-3.5 text-sm font-medium focus:outline-none focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 transition-all placeholder:text-zinc-600 shadow-inner text-white"
                    placeholder="My Server"
                  />
                </div>

                <div className="flex flex-col gap-2">
                  <label className="text-xs font-semibold text-zinc-400 uppercase tracking-wider pl-1">
                    {t('urlField')}
                  </label>
                  <input
                    type="text"
                    value={url}
                    onChange={(e) => {
                      setUrl(e.target.value);
                    }}
                    className="w-full bg-zinc-950/50 border border-white/10 rounded-xl px-4 py-3.5 text-sm focus:outline-none focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 transition-all placeholder:text-zinc-600 shadow-inner font-mono text-zinc-300"
                    placeholder="vless://..."
                  />
                </div>

                <button
                  onClick={handleManualAdd}
                  className="w-full bg-emerald-500 hover:bg-emerald-400 text-zinc-950 font-bold rounded-xl py-3.5 mt-4 flex items-center justify-center transition-all active:scale-[0.98] shadow-[0_0_20px_rgba(16,185,129,0.2)]"
                >
                  {t('addBtn')}
                </button>
              </div>
            </motion.div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
