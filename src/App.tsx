import { useEffect } from "react";
import { HashRouter, Route, Routes, useNavigate } from "react-router";
import { listen } from "@tauri-apps/api/event";
import { Toaster } from "sonner";
import { inTauri, type JobEvent } from "./lib/api";
import { useStore } from "./store";
import { Main } from "./routes/Main";
import { Transcript } from "./routes/Transcript";
import { Settings } from "./routes/Settings";

/** Ctrl+, opens Settings from anywhere (PDA UI-5). */
function Shortcuts() {
  const navigate = useNavigate();
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey && e.key === ",") {
        e.preventDefault();
        navigate("/settings");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [navigate]);
  return null;
}

export default function App() {
  useEffect(() => {
    const s = useStore.getState();
    const load = () => {
      void s.refreshStatus();
      void s.refresh();
      void s.refreshFolders();
    };
    load();
    if (!inTauri) return;
    const fail = (e: unknown) => console.error("event listener", e);
    const unlisten = [
      listen<JobEvent>("job", (e) => useStore.getState().applyEvent(e.payload)),
      // Missed events, or the core finished starting: read everything again.
      listen("jobs-stale", () => void useStore.getState().refresh()),
      listen("core-status", load),
    ].map((p) => p.catch((e) => (fail(e), () => {})));
    return () => unlisten.forEach((p) => void p.then((f) => f()));
  }, []);

  return (
    <HashRouter>
      <Shortcuts />
      <Routes>
        <Route path="/" element={<Main />} />
        <Route path="/job/:id" element={<Transcript />} />
        <Route path="/settings" element={<Settings />} />
      </Routes>
      <Toaster position="bottom-right" theme="system" toastOptions={{ className: "text-[12px]" }} />
    </HashRouter>
  );
}
