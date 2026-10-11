import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { CornerDownLeft } from "lucide-react";
import { api, errorText, type Preview } from "../lib/api";
import { useStore } from "../store";
import { Button } from "./ui/button";

const SINGLE = /(youtu\.be\/|[?&]v=|\/shorts\/|\/live\/|\/embed\/|instagram\.com\/(reel|reels|p|tv)\/)/;

function lines(text: string): string[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
}

/** Paste box: one link per line, Enter queues them (PDA IN-1, IN-5). */
export function LinkInput() {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [previews, setPreviews] = useState<Preview[]>([]);
  const [problems, setProblems] = useState<[string, string][]>([]);
  const box = useRef<HTMLTextAreaElement>(null);
  const refresh = useStore((s) => s.refresh);

  // Ctrl+V anywhere lands here.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const editing = e.target instanceof Element && e.target.closest("input, textarea, [contenteditable]");
      if (e.ctrlKey && e.key.toLowerCase() === "v" && !editing) box.current?.focus();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Channels, playlists and profiles: show their counts before queueing (IN-4).
  useEffect(() => {
    const collections = lines(text).filter((l) => !SINGLE.test(l) && /youtube\.com|instagram\.com/.test(l));
    if (collections.length === 0) {
      setPreviews([]);
      return;
    }
    let stale = false;
    const t = setTimeout(async () => {
      try {
        const p = await api.preview(collections);
        if (!stale) setPreviews(p);
      } catch {
        if (!stale) setPreviews([]);
      }
    }, 500);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [text]);

  async function submit() {
    const pasted = lines(text);
    if (pasted.length === 0 || busy) return;
    setBusy(true);
    try {
      const r = await api.enqueue(pasted);
      const bad = [...r.rejected, ...r.errors];
      setProblems(bad);
      // Keep only the lines that didn't go in, to fix and retry.
      setText(bad.map(([line]) => line).join("\n"));
      setPreviews([]);
      const added = r.added.length + r.retried.length;
      toast(`${added} added` + (r.already_done.length ? ` · ${r.already_done.length} done` : "") +
        (r.already_queued.length ? ` · ${r.already_queued.length} queued` : ""));
      await refresh();
    } catch (e) {
      setProblems([["", errorText(e)]]);
    } finally {
      setBusy(false);
    }
  }

  const rows = Math.min(6, Math.max(1, text.split("\n").length));
  return (
    <div className="flex min-w-0 flex-1 flex-col gap-1">
      <div className="flex items-start gap-1">
        <textarea
          ref={box}
          aria-label="Links"
          rows={rows}
          value={text}
          spellCheck={false}
          placeholder="Links"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void submit();
            }
          }}
          className="min-h-6 flex-1 resize-none rounded border border-line bg-bg px-2 py-[3px] font-mono text-[12px] leading-[18px] outline-none placeholder:text-muted focus:border-accent"
        />
        <Button variant="default" icon aria-label="Add" onClick={() => void submit()} disabled={busy || !text.trim()}>
          <CornerDownLeft size={13} />
        </Button>
      </div>
      {previews.length > 0 && (
        <div aria-live="polite" className="flex flex-wrap gap-x-3 gap-y-0.5 text-[12px] text-muted">
          {previews.map((p) =>
            p.error ? (
              <span key={p.input} className="text-bad">{p.error}</span>
            ) : (
              <span key={p.input}>
                <span className="text-fg">{p.title}</span> {p.total} · {p.new} new
              </span>
            ),
          )}
        </div>
      )}
      {problems.length > 0 && (
        <ul aria-live="polite" className="max-h-24 overflow-auto text-[12px] text-bad select-text">
          {problems.map(([line, reason], i) => (
            <li key={i} className="truncate">
              {line && <span className="font-mono">{line}</span>} {reason}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
