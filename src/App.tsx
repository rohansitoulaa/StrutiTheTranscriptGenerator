import { useEffect } from "react";
import { HashRouter, Route, Routes } from "react-router";
import { listen } from "@tauri-apps/api/event";
import { Toaster } from "sonner";
import { inTauri, type JobEvent } from "./lib/api";
import { useStore } from "./store";
import { Main } from "./routes/Main";
import { Transcript } from "./routes/Transcript";

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
      <Routes>
        <Route path="/" element={<Main />} />
        <Route path="/job/:id" element={<Transcript />} />
      </Routes>
      <Toaster position="bottom-right" theme="system" toastOptions={{ className: "text-[12px]" }} />
    </HashRouter>
  );
}
