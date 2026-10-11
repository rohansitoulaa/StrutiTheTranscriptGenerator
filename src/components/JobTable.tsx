import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router";
import {
  flexRender,
  getCoreRowModel,
  getFilteredRowModel,
  getSortedRowModel,
  useReactTable,
  type ColumnDef,
  type SortingState,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ArrowDown, ArrowUp, ExternalLink, FolderOpen, RefreshCw, RotateCcw, Sparkles, X } from "lucide-react";
import { toast } from "sonner";
import { api, errorText, type Job, type Status } from "../lib/api";
import { hasTranscript, isOpen, platform, progress, stateLabel, tone } from "../lib/jobs";
import { useStore } from "../store";
import { cn } from "../lib/utils";
import { StateDot } from "./StateDot";
import { Button } from "./ui/button";

const ROW = 26;
/** Rows that show a failure or wait reason under the title. */
const ROW_WITH_REASON = 38;

const showsReason = (j: Job) => Boolean(j.reason) && j.status !== "done";
const GRID = "grid-cols-[150px_minmax(200px,1fr)_160px_150px_120px]";

type StateFilter = "all" | "active" | Status;

async function act(run: () => Promise<unknown>) {
  try {
    await run();
    await useStore.getState().refresh();
  } catch (e) {
    toast.error(errorText(e));
  }
}

function Actions({ job }: { job: Job }) {
  return (
    <div className="flex justify-end gap-0.5 opacity-0 group-hover:opacity-100 group-focus-within:opacity-100">
      {(job.status === "failed" || job.status === "waiting") && (
        <Button icon aria-label="Retry" onClick={(e) => (e.stopPropagation(), act(() => api.retry(job.id)))}>
          <RotateCcw size={13} />
        </Button>
      )}
      {hasTranscript(job) && !isOpen(job) && (
        <Button icon aria-label="Re-clean" onClick={(e) => (e.stopPropagation(), act(() => api.reclean([job.video_key])))}>
          <Sparkles size={13} />
        </Button>
      )}
      {job.note_path && (
        <>
          <Button icon aria-label="Reveal" onClick={(e) => (e.stopPropagation(), act(() => api.reveal(job.id)))}>
            <FolderOpen size={13} />
          </Button>
          <Button icon aria-label="Open" onClick={(e) => (e.stopPropagation(), act(() => api.openNote(job.id)))}>
            <ExternalLink size={13} />
          </Button>
        </>
      )}
      {isOpen(job) && (
        <Button icon aria-label="Cancel" onClick={(e) => (e.stopPropagation(), act(() => api.cancel(job.id)))}>
          <X size={13} />
        </Button>
      )}
    </div>
  );
}

const columns: ColumnDef<Job>[] = [
  {
    id: "state",
    header: "State",
    accessorFn: (j) => stateLabel(j),
    cell: ({ row: { original: j } }) => (
      <div className="flex min-w-0 items-center gap-1.5">
        <StateDot tone={tone(j)} />
        <span className={cn("truncate", j.status === "failed" && "text-bad")}>
          {stateLabel(j)}
          {j.status === "done" && j.review && <span className="text-warn"> · review</span>}
        </span>
        {j.status === "running" && (
          <span className="ml-auto h-1 w-8 shrink-0 overflow-hidden rounded bg-line">
            <span className="block h-full bg-accent" style={{ width: `${progress(j) * 100}%` }} />
          </span>
        )}
      </div>
    ),
  },
  {
    id: "title",
    header: "Title",
    accessorFn: (j) => j.title ?? j.url,
    cell: ({ row: { original: j } }) => (
      <div className="min-w-0">
        <div className="truncate">{j.title ?? <span className="font-mono text-muted">{j.url}</span>}</div>
        {showsReason(j) && (
          <div className={cn("truncate text-[11px]", j.status === "failed" ? "text-bad" : "text-muted")}>{j.reason}</div>
        )}
      </div>
    ),
  },
  {
    id: "source",
    header: "Source",
    accessorFn: (j) => `${platform(j)} ${j.channel ?? ""}`,
    cell: ({ row: { original: j } }) => (
      <span className="flex min-w-0 items-center gap-1.5">
        <span className="shrink-0 text-[10px] font-medium tracking-wide text-muted uppercase">
          {platform(j) === "youtube" ? "YT" : "IG"}
        </span>
        <span className="truncate">{j.channel}</span>
      </span>
    ),
  },
  {
    id: "folder",
    header: "Folder",
    accessorFn: (j) => j.folder ?? "",
    cell: ({ getValue }) => <span className="truncate text-muted">{getValue<string>()}</span>,
  },
  {
    id: "actions",
    header: "",
    enableSorting: false,
    cell: ({ row: { original: j } }) => <Actions job={j} />,
  },
];

/** The queue (PDA UI-1 to UI-3): virtualised, filterable, sortable. */
export function JobTable() {
  const jobs = useStore((s) => s.jobs);
  const folderFilter = useStore((s) => s.folderFilter);
  const [query, setQuery] = useState("");
  const [state, setState] = useState<StateFilter>("all");
  const [source, setSource] = useState<"all" | "youtube" | "instagram">("all");
  const [sorting, setSorting] = useState<SortingState>([]);
  const filterBox = useRef<HTMLInputElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const navigate = useNavigate();

  // `/` focuses the filter (UI-3).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      if (e.key === "/" && !target?.closest("input, textarea")) {
        e.preventDefault();
        filterBox.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const rows = useMemo(
    () =>
      jobs.filter(
        (j) =>
          (state === "all" || (state === "active" ? isOpen(j) : j.status === state)) &&
          (source === "all" || platform(j) === source) &&
          (folderFilter === null || j.folder === folderFilter),
      ),
    [jobs, state, source, folderFilter],
  );

  const table = useReactTable({
    data: rows,
    columns,
    state: { sorting, globalFilter: query },
    onSortingChange: setSorting,
    onGlobalFilterChange: setQuery,
    globalFilterFn: (row, _id, value: string) => {
      const j = row.original;
      const hay = `${j.title ?? ""} ${j.channel ?? ""} ${j.folder ?? ""} ${j.video_key} ${j.input}`.toLowerCase();
      return hay.includes(value.toLowerCase());
    },
    getRowId: (j) => String(j.id),
    getCoreRowModel: getCoreRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
    getSortedRowModel: getSortedRowModel(),
  });

  const visible = table.getRowModel().rows;
  const virtual = useVirtualizer({
    count: visible.length,
    getScrollElement: () => scroller.current,
    estimateSize: (i) => (showsReason(visible[i].original) ? ROW_WITH_REASON : ROW),
    // Sizes follow the job, not its position, as filters reorder rows.
    getItemKey: (i) => visible[i].id,
    overscan: 12,
  });

  const failed = jobs.filter((j) => j.status === "failed").length;

  return (
    <section aria-label="Queue" className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-8 shrink-0 items-center gap-1.5 border-b border-line px-2">
        <input
          ref={filterBox}
          aria-label="Filter"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Filter"
          className="h-6 w-56 rounded border border-line bg-bg px-2 text-[12px] outline-none placeholder:text-muted focus:border-accent"
        />
        <select
          aria-label="State"
          value={state}
          onChange={(e) => setState(e.target.value as StateFilter)}
          className="h-6 rounded border border-line bg-bg px-1 text-[12px]"
        >
          {["all", "active", "queued", "running", "waiting", "failed", "done", "cancelled"].map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
        <select
          aria-label="Platform"
          value={source}
          onChange={(e) => setSource(e.target.value as typeof source)}
          className="h-6 rounded border border-line bg-bg px-1 text-[12px]"
        >
          <option value="all">all</option>
          <option value="youtube">YouTube</option>
          <option value="instagram">Instagram</option>
        </select>
        <span className="ml-auto text-[11px] text-muted tabular-nums">{visible.length}</span>
        {failed > 0 && (
          <Button variant="outline" onClick={() => act(() => api.retryFailed())}>
            <RefreshCw size={12} /> Retry {failed}
          </Button>
        )}
      </div>
      <div role="table" aria-rowcount={visible.length} className="flex min-h-0 flex-1 flex-col">
        <div role="row" className={cn("grid shrink-0 gap-3 border-b border-line px-3 py-1 text-[11px] font-medium text-muted", GRID)}>
          {table.getHeaderGroups()[0].headers.map((h) => (
            <div
              key={h.id}
              role="columnheader"
              aria-sort={h.column.getIsSorted() === "asc" ? "ascending" : h.column.getIsSorted() === "desc" ? "descending" : undefined}
            >
              {h.column.getCanSort() ? (
                <button type="button" className="flex items-center gap-0.5 hover:text-fg" onClick={h.column.getToggleSortingHandler()}>
                  {flexRender(h.column.columnDef.header, h.getContext())}
                  {h.column.getIsSorted() === "asc" && <ArrowUp size={10} />}
                  {h.column.getIsSorted() === "desc" && <ArrowDown size={10} />}
                </button>
              ) : null}
            </div>
          ))}
        </div>
        <div ref={scroller} className="min-h-0 flex-1 overflow-auto">
          <div style={{ height: virtual.getTotalSize(), position: "relative" }}>
            {virtual.getVirtualItems().map((v) => {
              const row = visible[v.index];
              const j = row.original;
              const openable = hasTranscript(j);
              return (
                <div
                  key={row.id}
                  role="row"
                  tabIndex={0}
                  aria-rowindex={v.index + 1}
                  onClick={() => openable && navigate(`/job/${j.id}`)}
                  onKeyDown={(e) => e.key === "Enter" && openable && navigate(`/job/${j.id}`)}
                  className={cn(
                    "group absolute inset-x-0 grid items-center gap-3 border-b border-line/50 px-3 text-[12px] hover:bg-hover",
                    GRID,
                  )}
                  style={{ height: v.size, transform: `translateY(${v.start}px)` }}
                >
                  {row.getVisibleCells().map((cell) => (
                    <div key={cell.id} role="cell" className="min-w-0">
                      {flexRender(cell.column.columnDef.cell, cell.getContext())}
                    </div>
                  ))}
                </div>
              );
            })}
          </div>
        </div>
      </div>
    </section>
  );
}
