import { useState } from 'react';
import { AnimatePresence, motion } from 'framer-motion';
import { useVPNStore } from '@/store.ts';
import { Edit2, Globe2, Loader2, RefreshCw, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { useTranslation } from '@/hooks/useTranslation.ts';
import type { Profile } from '@/types.ts';
import type { RefreshProgress } from '@/types.ts';
import type { TranslationKey } from '@/i18n.ts';
import * as z from 'zod';
import { AddProfileOptions } from '../AddProfileOptions';
import { ConfirmDialog } from '../ConfirmDialog';

const profileSchema = z.object({
  name: z.string().min(1, 'Name is required'),
  // eslint-disable-next-line @typescript-eslint/no-deprecated
  url: z.string().url('Invalid URL format'),
});

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  return `${(bytes / 1024 ** i).toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
}

export function Profiles() {
  const { profiles, addProfile, removeProfile, refreshProfile, updateProfile, refreshProgress } = useVPNStore();
  const { t } = useTranslation();

  const [editingId, setEditingId] = useState<string | null>(null);
  const [deleteConfirmId, setDeleteConfirmId] = useState<string | null>(null);

  const handleDelete = (id: string, _name: string) => {
    setDeleteConfirmId(id);
  };

  const confirmDelete = () => {
    if (!deleteConfirmId) return;
    const id = deleteConfirmId;
    setDeleteConfirmId(null);
    removeProfile(id)
      .then(() => {
        toast.success(t('profileDeleted'));
      })
      .catch(() => {
        toast.error(t('deleteFailed'));
      });
  };

  const handleRefresh = (profile: Profile) => {
    toast(`${t('updatingProfile')}${profile.name}...`);
    refreshProfile(profile.id)
      .then(() => {
        toast.success(`${t('profileStr')}${profile.name}${t('profileUpdated')}`);
      })
      .catch(() => {
        toast.error(t('refreshFailed'));
      });
  };

  return (
    <div className="flex flex-col h-full w-full p-10 pt-8">
      <header className="flex justify-between items-center mb-10">
        <div className="flex items-center gap-5">
          <div>
            <h1 className="text-3xl font-bold tracking-tight text-white">{t('navProfiles')}</h1>
            <p className="text-zinc-400 text-sm mt-1">{t('configManagement')}</p>
          </div>
        </div>
      </header>

      <div className="mb-8 flex justify-center w-full">
        <AddProfileOptions
          onAdd={(url, name) => {
            const promise = addProfile({
              name: name ?? 'New Profile',
              url,
            });
            toast.promise(promise, {
              loading: `${t('updatingProfile')}...`,
              success: t('profileAdded'),
              error: t('addFailed') || 'Failed to add profile',
            });
          }}
          horizontal={true}
          className="max-w-full"
        />
      </div>

      <div className="flex-1 overflow-y-auto overflow-x-hidden custom-scrollbar pb-8">
        <AnimatePresence>
          {profiles.map((p) => (
            <ProfileItem
              key={p.id}
              profile={p}
              progress={refreshProgress[p.id] ?? null}
              updateProfile={updateProfile}
              handleRefresh={handleRefresh}
              t={t}
              editingId={editingId}
              setEditingId={setEditingId}
              handleDelete={handleDelete}
            />
          ))}
        </AnimatePresence>

        {profiles.length === 0 && (
          <motion.div
            initial={{ opacity: 0, y: 10 }}
            animate={{ opacity: 1, y: 0 }}
            className="text-center py-12 flex flex-col items-center justify-center border-2 border-dashed border-white/5 rounded-3xl bg-zinc-900/20 backdrop-blur-sm"
          >
            <div className="w-20 h-20 bg-zinc-800/50 rounded-3xl flex items-center justify-center mb-5 shadow-inner">
              <Globe2 size={36} className="text-zinc-600" strokeWidth={1.5} />
            </div>
            <h3 className="text-zinc-200 font-semibold text-lg mb-2">{t('noProfiles')}</h3>
            <p className="text-zinc-500 text-sm max-w-xs leading-relaxed">{t('noProfilesDesc')}</p>
          </motion.div>
        )}
      </div>

      <ConfirmDialog
        open={deleteConfirmId !== null}
        title={t('deleteProfileConfirm')}
        description={profiles.find((p) => p.id === deleteConfirmId)?.name ?? ''}
        confirmLabel={t('delete')}
        cancelLabel={t('cancel')}
        onConfirm={confirmDelete}
        onCancel={() => {
          setDeleteConfirmId(null);
        }}
      />
    </div>
  );
}

interface ProfileItemProps {
  readonly profile: Profile;
  readonly progress: RefreshProgress | null;
  readonly updateProfile: (id: string, data: Partial<Profile>) => void;
  readonly handleRefresh: (profile: Profile) => void;
  readonly t: (key: TranslationKey) => string;
  readonly editingId: string | null;
  readonly setEditingId: (id: string | null) => void;
  readonly handleDelete: (id: string, name: string) => void;
}

function ProfileItem({
  profile: p,
  progress,
  updateProfile,
  handleRefresh,
  t,
  editingId,
  setEditingId,
  handleDelete,
}: ProfileItemProps) {
  const [editName, setEditName] = useState(p.name);
  const [editUrl, setEditUrl] = useState(p.url);
  // eslint-disable-next-line react-hooks/purity
  const timeAgo = p.lastUpdated ? Math.floor((Date.now() - p.lastUpdated) / 60000) : 0;

  const handleSave = () => {
    const result = profileSchema.safeParse({ name: editName, url: editUrl });
    if (!result.success) {
      toast.error(result.error.issues[0]?.message || 'Invalid form data');
      return;
    }
    updateProfile(p.id, { name: editName, url: editUrl });
    setEditingId(null);
  };

  return (
    <motion.div
      layout
      className="bg-zinc-900/40 backdrop-blur-md border border-white/5 hover:bg-zinc-900/60 hover:border-white/10 transition-all duration-300 p-6 rounded-3xl mb-5 flex flex-col justify-between group shadow-sm"
    >
      {editingId === p.id ? (
        <div className="flex flex-col gap-4 w-full">
          <input
            type="text"
            value={editName}
            onChange={(e) => {
              setEditName(e.target.value);
            }}
            className="bg-zinc-950/50 border border-white/10 rounded-xl px-4 py-3 text-sm font-medium focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none w-full transition-all shadow-inner"
          />
          <input
            type="text"
            value={editUrl}
            onChange={(e) => {
              setEditUrl(e.target.value);
            }}
            className="bg-zinc-950/50 border border-white/10 rounded-xl px-4 py-3 text-sm focus:ring-2 focus:ring-emerald-500/30 focus:border-emerald-500/50 outline-none w-full transition-all font-mono text-zinc-400 shadow-inner"
          />
          <div className="flex justify-end gap-3 mt-2">
            <button
              onClick={() => {
                setEditName(p.name);
                setEditUrl(p.url);
                setEditingId(null);
              }}
              className="px-5 py-2 rounded-xl text-sm font-medium transition-colors text-zinc-400 hover:text-white hover:bg-white/5"
            >
              {t('cancel')}
            </button>
            <button
              onClick={handleSave}
              className="bg-emerald-500 hover:bg-emerald-400 text-zinc-950 px-6 py-2 rounded-xl text-sm font-bold transition-all shadow-[0_0_15px_rgba(16,185,129,0.2)]"
            >
              {t('save')}
            </button>
          </div>
        </div>
      ) : (
        <>
          <div className="flex justify-between items-start mb-5">
            <div>
              <h3 className="font-bold text-xl text-zinc-100 flex items-center gap-3">
                <Globe2 size={20} className="text-emerald-500" strokeWidth={2.5} /> {p.name}
              </h3>
              <p className="text-xs text-zinc-500 font-mono mt-2 break-all max-w-2xl truncate bg-black/20 inline-block px-2 py-1 rounded-md border border-white/5">
                {p.url}
              </p>
              {p.servers.length > 0 && (
                <span className="inline-block mt-3 text-xs font-semibold text-emerald-400 bg-emerald-500/10 border border-emerald-500/20 px-3 py-1 rounded-lg">
                  {p.servers.length}{' '}
                  {p.servers.length === 1 ? t('serverSingular') : t('serverPlural')}
                </span>
              )}
              {p.trafficTotal && p.trafficTotal > 0 && (
                <div className="mt-3 flex items-center gap-3 text-[11px] font-mono text-zinc-400">
                  <span className="flex items-center gap-1">
                    <span className="text-zinc-500">{t('trafficUsed')}:</span>
                    <span className="text-emerald-400 font-semibold">
                      {formatBytes(p.trafficUsed ?? 0)} / {formatBytes(p.trafficTotal)}
                    </span>
                  </span>
                  {p.trafficExpiry && (
                    <span className="flex items-center gap-1">
                      <span className="text-zinc-500">·</span>
                      <span>
                        {t('trafficExpiry')}: {p.trafficExpiry}
                      </span>
                    </span>
                  )}
                </div>
              )}

              {/* ── Refresh progress bar ─────────────────────────────────── */}
              {progress && (
                <div className="mt-4">
                  <div className="flex items-center justify-between mb-2">
                    <span className="text-xs font-medium text-emerald-400 flex items-center gap-1.5">
                      <Loader2 size={12} className="animate-spin" />
                      {progress.phase === 'testing'
                        ? `${t('testingServers')}: ${progress.tested}/${progress.total}`
                        : progress.phase === 'tcp_ping'
                          ? `TCP Ping: ${progress.tested}/${progress.total}`
                          : progress.phase === 'singbox_starting'
                          ? t('startingSingbox')
                          : progress.phase === 'acquiring_ports'
                            ? t('acquiringPorts')
                            : progress.phase}
                    </span>
                    <span className="text-[11px] text-zinc-500">
                      {progress.cache_hits > 0 && (
                        <span className="text-emerald-500">
                          {progress.cache_hits}{' '}
                          {t('cached').toLowerCase()}
                          {' · '}
                        </span>
                      )}
                      {progress.successful}/{progress.total}{' '}
                      {t('successful').toLowerCase()}
                    </span>
                  </div>
                  {progress.total > 0 && (
                    <div className="w-full h-1.5 bg-zinc-800 rounded-full overflow-hidden">
                      <motion.div
                        className="h-full bg-gradient-to-r from-emerald-500 to-emerald-400 rounded-full"
                        initial={{ width: 0 }}
                        animate={{
                          width: `${Math.min(100, (progress.tested / progress.total) * 100)}%`,
                        }}
                        transition={{ duration: 0.3 }}
                      />
                    </div>
                  )}
                </div>
              )}
            </div>
          </div>

          <div className="flex items-center justify-between border-t border-white/5 pt-5 mt-2">
            <p className="text-xs text-zinc-500 font-medium flex items-center gap-1.5">
              <span className="w-1.5 h-1.5 rounded-full bg-zinc-600" />
              {t('updatedMinsAgo')}
              {timeAgo}
              {t('minsAgo')}
            </p>
            <div className="flex items-center gap-2.5 opacity-0 group-hover:opacity-100 transition-opacity duration-300">
              <button
                onClick={() => {
                  handleRefresh(p);
                }}
                disabled={progress !== null}
                title={t('refreshServers')}
                className={`p-2.5 border border-white/5 bg-zinc-800/50 rounded-xl hover:bg-zinc-700 hover:text-white text-zinc-400 transition-all shadow-sm disabled:opacity-50 disabled:cursor-not-allowed`}
              >
                {progress !== null ? (
                  <Loader2 size={16} className="animate-spin" />
                ) : (
                  <RefreshCw size={16} />
                )}
              </button>
              <button
                onClick={() => {
                  setEditingId(p.id);
                }}
                title={t('edit')}
                className="p-2.5 border border-white/5 bg-zinc-800/50 rounded-xl hover:bg-zinc-700 hover:text-white text-zinc-400 transition-all shadow-sm"
              >
                <Edit2 size={16} />
              </button>
              <button
                onClick={() => {
                  handleDelete(p.id, p.name);
                }}
                title={t('delete')}
                className="p-2.5 border border-red-500/20 bg-red-500/10 rounded-xl hover:bg-red-500/20 text-red-400 transition-all shadow-sm"
              >
                <Trash2 size={16} />
              </button>
            </div>
          </div>
        </>
      )}
    </motion.div>
  );
}
