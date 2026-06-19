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
      toast.error(t('clipboardErrMsg'), {
        description: t('clipboardErrDesc'),
        duration: 4000,
      });
    }
  };

  const handleManualAdd = () => {
    const result = manualSchema.safeParse({ name, url });
    if (!result.success) {
      const err = result.error.issues[0];
      toast.error(err.message);
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
        className={`flex items-center justify-center gap-4 ${horizontal ? 'flex-row' : 'flex-col sm:flex-row'}`}
      >
        <button
          onClick={() => {
            void handleClipboardAdd();
          }}
          className="flex-1 aspect-[4/3] max-h-32 bg-black/20 border border-white/10 hover:border-emerald-500/50 hover:bg-black/40 rounded-2xl flex flex-col items-center justify-center gap-4 transition-all group"
        >
          <Clipboard
            size={32}
            strokeWidth={1.5}
            className="text-blue-200 group-hover:text-emerald-400 group-hover:scale-110 transition-transform"
          />
          <span className="text-sm font-medium text-blue-100 group-hover:text-white transition-colors">
            {t('fromClipboard')}
          </span>
        </button>

        <button
          onClick={() => {
            setViewMode('manual');
          }}
          className="flex-1 aspect-[4/3] max-h-32 bg-black/20 border border-white/10 hover:border-emerald-500/50 hover:bg-black/40 rounded-2xl flex flex-col items-center justify-center gap-4 transition-all group"
        >
          <Plus
            size={32}
            strokeWidth={1.5}
            className="text-blue-200 group-hover:text-emerald-400 group-hover:scale-110 transition-transform"
          />
          <span className="text-sm font-medium text-blue-100 group-hover:text-white transition-colors">
            {t('manualAdd')}
          </span>
        </button>
      </div>

      <AnimatePresence>
        {viewMode === 'manual' && (
          <motion.div
            initial={{ opacity: 0, y: 20 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: 20 }}
            className="fixed inset-0 z-50 flex items-center justify-center p-8 bg-zinc-950/90 backdrop-blur-sm"
          >
            <div className="w-full max-w-md bg-zinc-900 border border-white/10 rounded-2xl shadow-2xl overflow-hidden flex flex-col text-left relative z-50">
              <div className="flex justify-between items-center px-6 py-4 border-b border-white/10">
                <h2 className="text-xl font-medium tracking-tight text-white">{t('manualAdd')}</h2>
                <button
                  onClick={() => {
                    setViewMode('initial');
                  }}
                  className="text-zinc-400 hover:text-white transition-colors"
                >
                  <X size={20} />
                </button>
              </div>

              <div className="p-6 flex flex-col gap-5">
                <div className="flex flex-col gap-1.5">
                  <label className="text-xs text-zinc-400 pl-1">{t('nameField')}</label>
                  <input
                    type="text"
                    value={name}
                    onChange={(e) => {
                      setName(e.target.value);
                    }}
                    className="w-full bg-zinc-800/50 border border-white/10 rounded-xl px-4 py-3 text-sm focus:outline-none focus:border-emerald-500/50 transition-colors placeholder:text-zinc-600 focus:bg-zinc-800"
                  />
                </div>

                <div className="flex flex-col gap-1.5">
                  <label className="text-xs text-zinc-400 pl-1">{t('urlField')}</label>
                  <input
                    type="text"
                    value={url}
                    onChange={(e) => {
                      setUrl(e.target.value);
                    }}
                    className="w-full bg-zinc-800/50 border border-white/10 rounded-xl px-4 py-3 text-sm focus:outline-none focus:border-emerald-500/50 transition-colors placeholder:text-zinc-600 focus:bg-zinc-800"
                  />
                </div>

                <button
                  onClick={() => {
                    handleManualAdd();
                  }}
                  className="w-full bg-blue-200 hover:bg-blue-300 text-blue-900 font-medium rounded-xl py-3 mt-2 flex items-center justify-center transition-all active:scale-[0.98]"
                >
                  {t('addBtn')}
                </button>
              </div>
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
