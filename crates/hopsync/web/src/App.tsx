// Decides what the daemon needs first: setup, a login, or the app. Any later 401 lands here too.

import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useState } from "react";
import { Toasts } from "./components/ui";
import { api, type AuthKind, message, onAuthRequired } from "./lib/api";
import { live } from "./lib/live";
import { LoginPage, SetupPage, UnreachablePage } from "./views/Auth";
import { Shell } from "./views/Shell";

type Screen = { kind: "loading" } | { kind: "error"; text: string } | { kind: AuthKind } | { kind: "app" };

export function App() {
  const [screen, setScreen] = useState<Screen>({ kind: "loading" });
  const queries = useQueryClient();

  const boot = useCallback(async () => {
    try {
      const health = await api<{ setup_required: boolean }>("GET", "/api/health");
      if (health.setup_required) {
        setScreen({ kind: "setup" });
        return;
      }
    } catch (e) {
      setScreen({ kind: "error", text: message(e) });
      return;
    }
    try {
      await api("GET", "/api/settings");
      setScreen({ kind: "app" });
    } catch {
      // `api` has already asked for a login.
    }
  }, []);

  useEffect(() => {
    onAuthRequired((kind) => setScreen({ kind }));
    void boot();
  }, [boot]);

  const inApp = screen.kind === "app";
  useEffect(() => {
    if (!inApp) return;
    live.connect();
    return () => {
      live.disconnect();
      // Nothing from a session outlives it.
      queries.clear();
    };
  }, [inApp, queries]);

  return (
    <>
      {screen.kind === "loading" && <div className="h-dvh" aria-busy="true" />}
      {screen.kind === "error" && <UnreachablePage text={screen.text} />}
      {screen.kind === "setup" && <SetupPage onDone={boot} />}
      {screen.kind === "login" && <LoginPage onDone={boot} />}
      {screen.kind === "app" && <Shell onLoggedOut={() => setScreen({ kind: "login" })} />}
      <Toasts />
    </>
  );
}
