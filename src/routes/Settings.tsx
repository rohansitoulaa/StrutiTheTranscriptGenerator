import { useCallback, useEffect, useState, type ReactNode } from "react";
import { useNavigate } from "react-router";
import { ArrowLeft, Check, Download, ExternalLink, RefreshCw, Trash2 } from "lucide-react";
import { toast } from "sonner";
import { api, errorText, type Check as HealthCheck, type Settings as Saved, type SettingsView, type YtDlpInfo } from "../lib/api";
import { useStore } from "../store";
import { Button } from "../components/ui/button";
import { StateDot } from "../components/StateDot";
import { cn } from "../lib/utils";

const PROVIDERS = ["lmstudio", "openai", "gemini", "custom"];
const BASE_URLS: Record<string, string> = {
  lmstudio: "http://localhost:1234/v1",
  openai: "https://api.openai.com/v1",
  gemini: "https://generativelanguage.googleapis.com/v1beta/openai",
  custom: "",
};
const BROWSERS = ["", "firefox", "chrome", "chromium", "brave", "edge", "opera", "vivaldi"];

const field = "h-[24px] w-full rounded border border-line bg-bg px-2 text-[12px] outline-none placeholder:text-muted focus:border-accent";

/** Settings (PDA UI-5): blank fields use the default shown in them. */
export function Settings() {
  const navigate = useNavigate();
  const [view, setView] = useState<SettingsView | null>(null);
  const [draft, setDraft] = useState<Saved>({});
  const [checks, setChecks] = useState<HealthCheck[] | null>(null);
  const [ytdlp, setYtdlp] = useState<YtDlpInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const refreshStatus = useStore((s) => s.refreshStatus);

  const load = useCallback(async () => {
    try {
      const v = await api.getSettings();
      setView(v);
      setDraft(v.settings);
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
    api.health().then(setChecks, (e) => setChecks([{ name: "Core", ok: false, detail: errorText(e) }]));
    api.ytdlpInfo().then(setYtdlp, () => setYtdlp(null));
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && navigate("/");
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [navigate]);

  const set = <K extends keyof Saved>(key: K, value: Saved[K]) => setDraft((d) => ({ ...d, [key]: value }));

  async function save() {
    setSaving(true);
    try {
      await api.saveSettings(draft);
      toast("Saved");
      await Promise.all([load(), refreshStatus()]);
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setSaving(false);
    }
  }

  const d = view?.defaults;
  const provider = draft.llm_provider ?? d?.llm_provider ?? "lmstudio";
  return (
    <div className="flex h-full flex-col">
      <header className="flex h-9 shrink-0 items-center gap-1.5 border-b border-line bg-panel px-2">
        <Button icon aria-label="Back" onClick={() => navigate("/")}>
          <ArrowLeft size={14} />
        </Button>
        <h1 className="text-[13px] font-medium">Settings</h1>
        <Button variant="default" className="ml-auto" disabled={!view || saving} onClick={() => void save()}>
          Save
        </Button>
      </header>
      {error && <div role="alert" className="px-3 py-2 font-mono text-[11px] text-bad select-text">{error}</div>}
      {view && d && (
        <div className="min-h-0 flex-1 overflow-auto">
          <div className="mx-auto grid max-w-5xl gap-x-8 gap-y-5 p-4 lg:grid-cols-2">
            <Section title="Status">
              {(checks ?? []).map((c) => (
                <div key={c.name} className="flex min-w-0 items-start gap-2 text-[12px]">
                  <span className="flex w-[88px] shrink-0 items-center gap-1.5">
                    <StateDot tone={c.ok ? "ok" : "bad"} />
                    {c.name}
                  </span>
                  <span className={cn("min-w-0 font-mono text-[11px] break-words select-text", c.ok ? "text-muted" : "text-bad")}>
                    {c.detail}
                  </span>
                </div>
              ))}
            </Section>

            <Section title="LLM">
              <Row label="Provider">
                <select className={field} value={provider} onChange={(e) => set("llm_provider", e.target.value)}>
                  {PROVIDERS.map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))}
                </select>
              </Row>
              <Row label="Base URL">
                <input
                  className={field}
                  value={draft.llm_base_url ?? ""}
                  placeholder={BASE_URLS[provider] || d.llm_base_url}
                  onChange={(e) => set("llm_base_url", e.target.value)}
                />
              </Row>
              <ModelRow
                provider={provider}
                baseUrl={draft.llm_base_url ?? null}
                value={draft.llm_model ?? ""}
                placeholder={d.llm_model}
                onChange={(v) => set("llm_model", v)}
              />
              {provider !== "lmstudio" && (
                <>
                  <KeyRow provider={provider} stored={view.keys.some(([p, has]) => p === provider && has)} onChange={load} />
                  <Row label="Parallel">
                    <NumberInput value={draft.llm_concurrency} placeholder={2} onChange={(v) => set("llm_concurrency", v)} />
                  </Row>
                </>
              )}
            </Section>

            <Section title="Cleanup">
              <Row label="Prompt" top>
                <textarea
                  className={cn(field, "h-32 resize-y py-1 font-mono text-[11px] leading-snug")}
                  value={draft.cleanup_prompt ?? ""}
                  placeholder={d.cleanup_prompt}
                  onChange={(e) => set("cleanup_prompt", e.target.value)}
                />
              </Row>
              <Row label="Chunk tokens">
                <NumberInput value={draft.chunk_tokens} placeholder={d.chunk_tokens} onChange={(v) => set("chunk_tokens", v)} />
              </Row>
              <Row label="Min ratio">
                <NumberInput step={0.05} value={draft.min_ratio} placeholder={d.min_ratio} onChange={(v) => set("min_ratio", v)} />
              </Row>
              <Row label="Max ratio">
                <NumberInput step={0.05} value={draft.max_ratio} placeholder={d.max_ratio} onChange={(v) => set("max_ratio", v)} />
              </Row>
              <Row label="Max translated">
                <NumberInput
                  step={0.05}
                  value={draft.max_ratio_translated}
                  placeholder={d.max_ratio_translated}
                  onChange={(v) => set("max_ratio_translated", v)}
                />
              </Row>
            </Section>

            <Section title="Storage">
              <Row label="Folder">
                <input className={field} value={draft.out_dir ?? ""} placeholder={d.out_dir} onChange={(e) => set("out_dir", e.target.value)} />
              </Row>
              <Row label="Postgres">
                <input
                  className={field}
                  value={draft.database_url ?? ""}
                  placeholder={d.database_url}
                  onChange={(e) => set("database_url", e.target.value)}
                />
              </Row>
            </Section>

            <Section title="Whisper">
              <Row label="Model">
                <input
                  className={field}
                  value={draft.whisper_model ?? ""}
                  placeholder={d.whisper_model}
                  onChange={(e) => set("whisper_model", e.target.value)}
                />
              </Row>
              <Row label="GPU">
                <input className={field} value={draft.gpu ?? ""} placeholder={d.gpu} onChange={(e) => set("gpu", e.target.value)} />
              </Row>
            </Section>

            <Section title="Instagram">
              <Row label="Cookies">
                <select className={field} value={draft.cookies_browser ?? ""} onChange={(e) => set("cookies_browser", e.target.value)}>
                  {BROWSERS.map((b) => (
                    <option key={b} value={b}>
                      {b || "none"}
                    </option>
                  ))}
                </select>
              </Row>
              <Row label="Delay s">
                <NumberInput value={draft.ig_delay_secs} placeholder={d.ig_delay_secs} onChange={(v) => set("ig_delay_secs", v)} />
              </Row>
              <Row label="Batch">
                <NumberInput value={draft.ig_batch} placeholder={d.ig_batch} onChange={(v) => set("ig_batch", v)} />
              </Row>
              <Row label="Pause s">
                <NumberInput value={draft.ig_pause_secs} placeholder={d.ig_pause_secs} onChange={(v) => set("ig_pause_secs", v)} />
              </Row>
            </Section>

            <Section title="yt-dlp">
              <YtDlpRow info={ytdlp} onUpdated={load} />
            </Section>

            <Section title="Log">
              <div className="flex min-w-0 items-center gap-2 text-[12px]">
                <span className="min-w-0 truncate font-mono text-[11px] text-muted select-text">{view.log_file}</span>
                <Button icon aria-label="Open log" onClick={() => api.openLog().catch((e) => toast.error(errorText(e)))}>
                  <ExternalLink size={13} />
                </Button>
              </div>
            </Section>
          </div>
        </div>
      )}
    </div>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  const id = `section-${title}`;
  return (
    <section aria-labelledby={id} className="flex min-w-0 flex-col gap-1.5">
      <h2 id={id} className="text-[11px] font-medium tracking-wide text-muted uppercase">
        {title}
      </h2>
      {children}
    </section>
  );
}

function Row({ label, top, children }: { label: string; top?: boolean; children: ReactNode }) {
  return (
    <label className={cn("grid grid-cols-[96px_1fr] gap-2 text-[12px]", top ? "items-start" : "items-center")}>
      <span className="text-muted">{label}</span>
      <span className="min-w-0">{children}</span>
    </label>
  );
}

function NumberInput({
  value,
  placeholder,
  step,
  onChange,
}: {
  value: number | null | undefined;
  placeholder: number;
  step?: number;
  onChange: (v: number | null) => void;
}) {
  return (
    <input
      type="number"
      min={0}
      step={step ?? 1}
      className={field}
      value={value ?? ""}
      placeholder={String(placeholder)}
      onChange={(e) => onChange(e.target.value === "" ? null : Number(e.target.value))}
    />
  );
}

/** Model names come from the provider (LLM-3); Test sends a tiny prompt (LLM-4). */
function ModelRow(props: {
  provider: string;
  baseUrl: string | null;
  value: string;
  placeholder: string;
  onChange: (v: string) => void;
}) {
  const { provider, baseUrl, value, placeholder, onChange } = props;
  const [models, setModels] = useState<string[]>([]);
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);
  const [busy, setBusy] = useState(false);

  async function fetchModels() {
    try {
      setModels(await api.listModels(provider, baseUrl));
      setResult(null);
    } catch (e) {
      setResult({ ok: false, text: errorText(e) });
    }
  }

  async function test() {
    setBusy(true);
    setResult(null);
    try {
      const ms = await api.testLlm(provider, value || placeholder, baseUrl);
      setResult({ ok: true, text: `${(ms / 1000).toFixed(1)} s` });
    } catch (e) {
      setResult({ ok: false, text: errorText(e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <Row label="Model">
        <span className="flex gap-1">
          <input list="models" className={field} value={value} placeholder={placeholder} onChange={(e) => onChange(e.target.value)} />
          <datalist id="models">
            {models.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
          <Button icon variant="outline" aria-label="Load models" onClick={() => void fetchModels()}>
            <RefreshCw size={12} />
          </Button>
          <Button variant="outline" disabled={busy} onClick={() => void test()}>
            Test
          </Button>
        </span>
      </Row>
      {result && (
        <div aria-live="polite" className={cn("pl-[104px] font-mono text-[11px] break-words select-text", result.ok ? "text-ok" : "text-bad")}>
          {result.ok && <Check size={11} className="mr-1 inline" />}
          {result.text}
        </div>
      )}
    </>
  );
}

/** Keys go straight to the keyring and are never shown again (LLM-5). */
function KeyRow({ provider, stored, onChange }: { provider: string; stored: boolean; onChange: () => Promise<void> }) {
  const [key, setKey] = useState("");
  const run = async (f: () => Promise<void>) => {
    try {
      await f();
      setKey("");
      await onChange();
    } catch (e) {
      toast.error(errorText(e));
    }
  };
  return (
    <Row label="API key">
      <span className="flex items-center gap-1">
        <input
          type="password"
          autoComplete="off"
          className={field}
          value={key}
          placeholder={stored ? "stored" : ""}
          onChange={(e) => setKey(e.target.value)}
        />
        <Button variant="outline" disabled={!key.trim()} onClick={() => void run(() => api.setApiKey(provider, key))}>
          Store
        </Button>
        {stored && (
          <Button icon variant="outline" aria-label="Remove key" onClick={() => void run(() => api.removeApiKey(provider))}>
            <Trash2 size={12} />
          </Button>
        )}
      </span>
    </Row>
  );
}

/** Version in use and its age (YT-2); Update installs the latest (YT-3). */
function YtDlpRow({ info, onUpdated }: { info: YtDlpInfo | null; onUpdated: () => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const outdated = info?.latest != null && info.version != null && info.latest !== info.version;

  async function update() {
    setBusy(true);
    try {
      const version = await api.updateYtdlp();
      toast(version);
      await onUpdated();
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  if (!info) return null;
  return (
    <div id="ytdlp" className="flex min-w-0 flex-col gap-1 text-[12px]">
      <div className="flex items-center gap-2">
        <span className={cn(info.error && "text-bad")}>{info.version ?? info.error}</span>
        {info.age_days != null && <span className="text-muted">{info.age_days} d</span>}
        {info.latest && outdated && <span className="text-warn-text">→ {info.latest}</span>}
        <Button variant={outdated ? "default" : "outline"} className="ml-auto" disabled={busy} onClick={() => void update()}>
          <Download size={12} /> Update
        </Button>
      </div>
      <span className="truncate font-mono text-[11px] text-muted select-text">{info.path}</span>
    </div>
  );
}
