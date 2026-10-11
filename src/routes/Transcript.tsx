import { useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router";
import { ArrowLeft, Columns2, ExternalLink, FolderOpen, Sparkles } from "lucide-react";
import { toast } from "sonner";
import { api, errorText, type TranscriptView } from "../lib/api";
import { useStore } from "../store";
import { Button } from "../components/ui/button";
import { cn } from "../lib/utils";

/** Cleaned note, with the raw transcript beside it on demand (PDA UI-4). */
export function Transcript() {
  const { id } = useParams();
  const navigate = useNavigate();
  const [view, setView] = useState<TranscriptView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [raw, setRaw] = useState(false);
  const jobs = useStore((s) => s.jobs);
  const job = jobs.find((j) => j.id === Number(id));

  // A different job: never show the previous one's text meanwhile.
  useEffect(() => {
    setView(null);
    setError(null);
  }, [id]);

  useEffect(() => {
    if (!id) return;
    let stale = false;
    api
      .transcript(Number(id))
      .then((v) => !stale && (setView(v), setError(null)))
      .catch((e) => !stale && setError(errorText(e)));
    return () => {
      stale = true;
    };
    // Reload when the job's state changes (a re-clean finishing).
  }, [id, job?.status, job?.stage]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && navigate("/");
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [navigate]);

  const run = (f: () => Promise<unknown>) => f().catch((e) => toast.error(errorText(e)));
  const showRaw = raw || !view?.cleaned;

  return (
    <div className="flex h-full flex-col">
      <header className="flex h-9 shrink-0 items-center gap-1.5 border-b border-line bg-panel px-2">
        <Button icon aria-label="Back" onClick={() => navigate("/")}>
          <ArrowLeft size={14} />
        </Button>
        <h1 className="min-w-0 truncate text-[13px] font-medium">{view?.title ?? job?.title ?? ""}</h1>
        {view && (
          <span className="shrink-0 text-[11px] text-muted">
            {[view.language, view.source, view.model, view.review ? "review" : null].filter(Boolean).join(" · ")}
          </span>
        )}
        <div className="ml-auto flex gap-0.5">
          <Button
            icon
            aria-label="Raw"
            aria-pressed={raw}
            disabled={!view?.cleaned}
            onClick={() => setRaw(!raw)}
            className={cn(raw && "bg-hover text-accent")}
          >
            <Columns2 size={13} />
          </Button>
          {view && (
            <Button icon aria-label="Re-clean" onClick={() => run(() => api.reclean([view.key]))}>
              <Sparkles size={13} />
            </Button>
          )}
          {view?.note_path && (
            <>
              <Button icon aria-label="Reveal" onClick={() => run(() => api.reveal(view.id))}>
                <FolderOpen size={13} />
              </Button>
              <Button icon aria-label="Open" onClick={() => run(() => api.openNote(view.id))}>
                <ExternalLink size={13} />
              </Button>
            </>
          )}
        </div>
      </header>
      {error && <div className="px-3 py-2 font-mono text-[11px] text-bad select-text">{error}</div>}
      {view && (
        <div className={cn("grid min-h-0 flex-1", view.cleaned && showRaw ? "grid-cols-2 divide-x divide-line" : "grid-cols-1")}>
          {view.cleaned && <Text text={view.cleaned} label="Cleaned" />}
          {showRaw && <Text text={view.raw} label="Raw" lang={view.language ?? undefined} />}
        </div>
      )}
    </div>
  );
}

function Text({ text, label, lang }: { text: string; label: string; lang?: string }) {
  return (
    <article aria-label={label} lang={lang} className="min-h-0 overflow-auto px-6 py-4 select-text">
      <div className="mx-auto max-w-[72ch] space-y-3 font-mono text-[12.5px] leading-relaxed whitespace-pre-wrap">
        {text.split(/\n{2,}/).map((p, i) => (
          <p key={i}>{p}</p>
        ))}
      </div>
    </article>
  );
}
