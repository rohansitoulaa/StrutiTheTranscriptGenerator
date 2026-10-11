import { Folder, Inbox } from "lucide-react";
import { useStore } from "../store";
import { cn } from "../lib/utils";

/** Read-only view of `Shruti/` (PDA FO-12); a click narrows the queue. */
export function FolderTree() {
  const folders = useStore((s) => s.folders);
  const selected = useStore((s) => s.folderFilter);
  const select = useStore((s) => s.setFolderFilter);
  const pick = (path: string) => select(selected === path ? null : path);

  const Item = ({ path, label, count, icon }: { path: string; label: string; count: number; icon?: boolean }) => (
    <button
      type="button"
      onClick={() => pick(path)}
      aria-pressed={selected === path}
      className={cn(
        "flex h-[22px] w-full items-center gap-1.5 rounded px-1.5 text-left text-[12px] hover:bg-hover",
        selected === path && "bg-hover text-accent",
      )}
    >
      {icon && <Inbox size={12} className="shrink-0 text-muted" />}
      <span className="truncate">{label}</span>
      <span className="ml-auto text-[11px] text-muted tabular-nums">{count}</span>
    </button>
  );

  return (
    <nav aria-label="Folders" className="flex flex-col gap-1 overflow-auto p-1.5">
      <Item path="Inbox" label="Inbox" count={folders.inbox} icon />
      {folders.areas.map((area) => (
        <div key={area.name}>
          <div className="flex h-[22px] items-center gap-1.5 px-1.5 text-[11px] font-medium text-muted">
            <Folder size={12} className="shrink-0" />
            <span className="truncate">{area.name}</span>
          </div>
          <div className="pl-3">
            {area.topics.map((t) => (
              <Item key={t.path} path={t.path} label={t.name} count={t.notes} />
            ))}
          </div>
        </div>
      ))}
    </nav>
  );
}
