import type { Tone } from "../lib/jobs";
import { cn } from "../lib/utils";

const colors: Record<Tone, string> = {
  idle: "bg-idle",
  run: "bg-accent motion-safe:animate-pulse",
  ok: "bg-ok",
  warn: "bg-warn",
  bad: "bg-bad",
};

/** Never colour alone: always next to its label (PDA UI-1). */
export function StateDot({ tone }: { tone: Tone }) {
  return <span aria-hidden className={cn("inline-block size-2 shrink-0 rounded-full", colors[tone])} />;
}
