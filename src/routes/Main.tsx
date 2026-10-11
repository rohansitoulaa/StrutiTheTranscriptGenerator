import { Pause, Play } from "lucide-react";
import { toast } from "sonner";
import { api, errorText } from "../lib/api";
import { useStore } from "../store";
import { FolderTree } from "../components/FolderTree";
import { JobTable } from "../components/JobTable";
import { LinkInput } from "../components/LinkInput";
import { Button } from "../components/ui/button";

export function Main() {
  const status = useStore((s) => s.status);
  const loadError = useStore((s) => s.loadError);
  const refreshStatus = useStore((s) => s.refreshStatus);
  const paused = status?.paused ?? false;

  async function togglePause() {
    try {
      await api.setPaused(!paused);
      await refreshStatus();
    } catch (e) {
      toast.error(errorText(e));
    }
  }

  const problem = status?.error ?? loadError;
  return (
    <div className="flex h-full flex-col">
      <header className="flex shrink-0 items-start gap-2 border-b border-line bg-panel p-2">
        <LinkInput />
        <Button icon aria-label={paused ? "Resume" : "Pause"} onClick={() => void togglePause()}>
          {paused ? <Play size={13} /> : <Pause size={13} />}
        </Button>
      </header>
      {problem && (
        <div role="alert" className="shrink-0 border-b border-line bg-bad/10 px-3 py-1 font-mono text-[11px] text-bad select-text">
          {problem}
        </div>
      )}
      <div className="flex min-h-0 flex-1">
        <aside className="w-52 shrink-0 overflow-auto border-r border-line bg-panel">
          <FolderTree />
        </aside>
        <JobTable />
      </div>
    </div>
  );
}
