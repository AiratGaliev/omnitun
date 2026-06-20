import { AnimatePresence, motion } from 'framer-motion';

interface ConfirmDialogProps {
  readonly open: boolean;
  readonly title: string;
  readonly description: string;
  readonly confirmLabel?: string;
  readonly cancelLabel?: string;
  readonly onConfirm: () => void;
  readonly onCancel: () => void;
}

export function ConfirmDialog({
  open,
  title,
  description,
  confirmLabel = 'Delete',
  cancelLabel = 'Cancel',
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  return (
    <AnimatePresence>
      {open && (
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
            className="w-full max-w-sm bg-zinc-900/95 backdrop-blur-2xl border border-white/10 rounded-3xl shadow-2xl overflow-hidden text-left relative z-50"
          >
            <div className="p-8 flex flex-col gap-4">
              <h2 className="text-lg font-bold tracking-tight text-white">{title}</h2>
              <p className="text-sm text-zinc-400 leading-relaxed">{description}</p>
              <div className="flex justify-end gap-3 mt-4">
                <button
                  onClick={onCancel}
                  className="px-5 py-2.5 rounded-xl text-sm font-medium transition-colors text-zinc-400 hover:text-white hover:bg-white/5"
                >
                  {cancelLabel}
                </button>
                <button
                  onClick={onConfirm}
                  className="bg-red-500 hover:bg-red-400 text-white px-6 py-2.5 rounded-xl text-sm font-bold transition-all"
                >
                  {confirmLabel}
                </button>
              </div>
            </div>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
