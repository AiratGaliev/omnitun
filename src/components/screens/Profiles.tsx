import {useState} from 'react';
import {AnimatePresence, motion} from 'framer-motion';
import {useVPNStore} from '../../store';
import {Edit2, Globe2, RefreshCw, Trash2} from 'lucide-react';
import {toast} from 'sonner';
import {useTranslation} from '../../hooks/useTranslation';
import type {Profile} from '../../types';
import type {TranslationKey} from '../../i18n';
import * as z from 'zod';
import {AddProfileOptions} from '../AddProfileOptions';

const profileSchema = z.object({
  name: z.string().min(1, 'Name is required'),
  // eslint-disable-next-line @typescript-eslint/no-deprecated
  url: z.string().url('Invalid URL format'),
});

export function Profiles() {
  const {profiles, addProfile, removeProfile, refreshProfile, updateProfile} = useVPNStore();
  const {t} = useTranslation();

  const [editingId, setEditingId] = useState<string | null>(null);

  const handleRefresh = (profile: Profile) => {
    toast(`${t('updatingProfile')}${profile.name}...`);
    refreshProfile(profile.id)
    .then(() => {
      toast.success(`${t('profileStr')}${profile.name}${t('profileUpdated')}`);
    })
    .catch(() => {
      toast.error('Failed to refresh subscription');
    });
  };

  const handleDelete = (id: string, name: string) => {
    if (confirm(`${t('deleteProfileConfirm')}${name}?`)) {
      removeProfile(id)
      .then(() => {
        toast.success(t('profileDeleted'));
      })
      .catch(() => {
        toast.error('Failed to delete profile');
      });
    }
  };

  return (
      <div className="flex flex-col h-full w-full p-8">
        <header className="flex justify-between items-center mb-8">
          <div className="flex items-center gap-4">
            <div
                className="w-12 h-12 bg-white/5 border border-white/10 rounded-xl flex items-center justify-center shadow-lg">
              <Globe2 size={24} className="text-zinc-300"/>
            </div>
            <div>
              <h1 className="text-2xl font-bold tracking-tight">{t('navProfiles')}</h1>
              <p className="text-zinc-500 text-sm">{t('configManagement')}</p>
            </div>
          </div>
        </header>

        <div className="mb-6 flex justify-center w-full">
          <AddProfileOptions
              onAdd={(url, name) => {
                void addProfile({name: name ?? 'New Profile', url}).then(() => {
                  toast.success(t('profileAdded'));
                });
              }}
              horizontal={true}
              className="max-w-full"
          />
        </div>

        <div className="flex-1 overflow-y-auto overflow-x-hidden custom-scrollbar pb-4">
          <AnimatePresence>
            {profiles.map((p) => (
                <ProfileItem
                    key={p.id}
                    profile={p}
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
                  initial={{opacity: 0, y: 10}}
                  animate={{opacity: 1, y: 0}}
                  className="text-center py-16 flex flex-col items-center justify-center border border-dashed border-white/10 rounded-2xl bg-white/[0.02]"
              >
                <div
                    className="w-16 h-16 bg-white/5 rounded-2xl flex items-center justify-center mb-4">
                  <Globe2 size={28} className="text-zinc-600"/>
                </div>
                <h3 className="text-zinc-300 font-medium mb-1">{t('noProfiles')}</h3>
                <p className="text-zinc-500 text-sm max-w-[250px]">{t('noProfilesDesc')}</p>
              </motion.div>
          )}
        </div>
      </div>
  );
}

interface ProfileItemProps {
  readonly profile: Profile;
  readonly updateProfile: (id: string, data: Partial<Profile>) => void;
  readonly handleRefresh: (profile: Profile) => void;
  readonly t: (key: TranslationKey) => string;
  readonly editingId: string | null;
  readonly setEditingId: (id: string | null) => void;
  readonly handleDelete: (id: string, name: string) => void;
}

function ProfileItem({
                       profile: p,
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
    const result = profileSchema.safeParse({name: editName, url: editUrl});
    if (!result.success) {
      toast.error(result.error.issues[0]?.message || 'Invalid form data');
      return;
    }
    updateProfile(p.id, {name: editName, url: editUrl});
    setEditingId(null);
  };

  return (
      <motion.div
          layout
          className="bg-white/5 border border-white/10 p-5 rounded-2xl mb-4 flex flex-col justify-between group"
      >
        {editingId === p.id ? (
            <div className="flex flex-col gap-3 w-full">
              <input
                  type="text"
                  value={editName}
                  onChange={(e) => {
                    setEditName(e.target.value);
                  }}
                  className="bg-black/40 border border-white/10 rounded-lg px-3 py-2 text-sm focus:ring-2 focus:ring-emerald-500/20 focus:border-emerald-500/50 outline-none w-full transition-all"
              />
              <input
                  type="text"
                  value={editUrl}
                  onChange={(e) => {
                    setEditUrl(e.target.value);
                  }}
                  className="bg-black/40 border border-white/10 rounded-lg px-3 py-2 text-sm focus:ring-2 focus:ring-emerald-500/20 focus:border-emerald-500/50 outline-none w-full transition-all font-mono text-zinc-300"
              />
              <div className="flex justify-end gap-2 mt-2">
                <button
                    onClick={() => {
                      setEditName(p.name);
                      setEditUrl(p.url);
                      setEditingId(null);
                    }}
                    className="px-4 py-1.5 rounded-lg text-sm transition-colors text-zinc-400 hover:text-white"
                >
                  {t('cancel')}
                </button>
                <button
                    onClick={handleSave}
                    className="bg-emerald-500/20 text-emerald-400 border border-emerald-500/30 px-4 py-1.5 rounded-lg text-sm font-medium hover:bg-emerald-500/30 transition-colors"
                >
                  {t('save')}
                </button>
              </div>
            </div>
        ) : (
            <>
              <div className="flex justify-between items-start mb-4">
                <div>
                  <h3 className="font-bold text-lg text-zinc-100 flex items-center gap-2">
                    <Globe2 size={18} className="text-emerald-500"/> {p.name}
                  </h3>
                  <p className="text-xs text-zinc-500 font-mono mt-1 break-all max-w-[400px] truncate">
                    {p.url}
                  </p>
                  {p.servers.length > 0 && (
                      <span
                          className="inline-block mt-1.5 text-[10px] text-emerald-400/80 bg-emerald-500/10 px-2 py-0.5 rounded-full">
                  {p.servers.length} {p.servers.length === 1 ? 'server' : 'servers'}
                </span>
                  )}
                </div>
              </div>

              <div className="flex items-center justify-between border-t border-white/5 pt-4 mt-2">
                <p className="text-[11px] text-zinc-500 font-medium">
                  {t('updatedMinsAgo')}
                  {timeAgo}
                  {t('minsAgo')}
                </p>
                <div className="flex items-center gap-2">
                  <button
                      onClick={() => {
                        handleRefresh(p);
                      }}
                      title={t('refreshServers')}
                      className="p-2 border border-white/5 bg-black/20 rounded-lg hover:bg-white/10 text-zinc-400 transition-colors"
                  >
                    <RefreshCw size={14}/>
                  </button>
                  <button
                      onClick={() => {
                        setEditingId(p.id);
                      }}
                      title={t('edit')}
                      className="p-2 border border-white/5 bg-black/20 rounded-lg hover:bg-white/10 text-zinc-400 transition-colors"
                  >
                    <Edit2 size={14}/>
                  </button>
                  <button
                      onClick={() => {
                        handleDelete(p.id, p.name);
                      }}
                      title={t('delete')}
                      className="p-2 border border-red-500/20 bg-red-500/10 rounded-lg hover:bg-red-500/20 text-red-400 transition-colors"
                  >
                    <Trash2 size={14}/>
                  </button>
                </div>
              </div>
            </>
        )}
      </motion.div>
  );
}
