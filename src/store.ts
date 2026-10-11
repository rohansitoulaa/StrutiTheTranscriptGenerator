import { create } from "zustand";
import { api, errorText, type AppStatus, type Folders, type Job, type JobEvent } from "./lib/api";

interface Store {
  jobs: Job[];
  status: AppStatus | null;
  folders: Folders;
  /** `Area/Topic` or `Inbox` the queue is narrowed to. */
  folderFilter: string | null;
  loadError: string | null;
  refresh: () => Promise<void>;
  refreshSoon: () => void;
  refreshFolders: () => Promise<void>;
  refreshStatus: () => Promise<void>;
  applyEvent: (e: JobEvent) => void;
  setFolderFilter: (folder: string | null) => void;
}

let refreshTimer: ReturnType<typeof setTimeout> | null = null;

export const useStore = create<Store>((set, get) => ({
  jobs: [],
  status: null,
  folders: { inbox: 0, areas: [] },
  folderFilter: null,
  loadError: null,

  refresh: async () => {
    try {
      set({ jobs: await api.jobs(), loadError: null });
    } catch (e) {
      set({ loadError: errorText(e) });
    }
  },

  // Events carry stage and status only; titles, folders and note paths
  // come with the next full read, batched while a queue is busy.
  refreshSoon: () => {
    if (refreshTimer) return;
    refreshTimer = setTimeout(() => {
      refreshTimer = null;
      void get().refresh();
    }, 800);
  },

  refreshFolders: async () => {
    try {
      set({ folders: await api.folders() });
    } catch {
      // The status line already shows why the core isn't ready.
    }
  },

  refreshStatus: async () => {
    try {
      set({ status: await api.status() });
    } catch (e) {
      set({ loadError: errorText(e) });
    }
  },

  applyEvent: (e) => {
    const jobs = get().jobs;
    const i = jobs.findIndex((j) => j.id === e.job_id);
    if (i >= 0) {
      const next = jobs.slice();
      next[i] = { ...jobs[i], stage: e.stage, status: e.status, reason: e.reason };
      set({ jobs: next });
    }
    get().refreshSoon();
    if (e.status === "done") void get().refreshFolders();
  },

  setFolderFilter: (folder) => set({ folderFilter: folder }),
}));
