// The frame around every page: navigation, connection state, theme, logout, and the tab title.

import { ArrowLeftRight, LogOut, Moon, type LucideIcon, Server, Settings as SettingsIcon, Sun, WifiOff } from "lucide-react";
import { useEffect, useState } from "react";
import { api, message } from "../lib/api";
import { rate } from "../lib/format";
import { useJobSync } from "../lib/hooks";
import { useLive } from "../lib/live";
import { cn, prefs, toast, useHash } from "../lib/util";
import { JobPage } from "./Job";
import { ServersPage } from "./Servers";
import { SettingsPage } from "./Settings";
import { TransfersPage } from "./Transfers";

const NAV: { id: string; label: string; icon: LucideIcon }[] = [
  { id: "transfers", label: "Transfers", icon: ArrowLeftRight },
  { id: "servers", label: "Servers", icon: Server },
  { id: "settings", label: "Settings", icon: SettingsIcon },
];

function useTheme() {
  const [theme, setTheme] = useState(document.documentElement.dataset.theme === "light" ? "light" : "dark");
  return {
    theme,
    toggle() {
      const next = theme === "light" ? "dark" : "light";
      document.documentElement.dataset.theme = next;
      prefs.set("theme", next);
      setTheme(next);
    },
  };
}

/** Speed in the tab title, so a transfer can be watched from another tab. */
function useTitleSpeed() {
  const { up, down } = useLive((s) => s.totals());
  useEffect(() => {
    const parts = [down ? `↓ ${rate(down)}` : "", up ? `↑ ${rate(up)}` : ""].filter(Boolean);
    document.title = parts.length ? `${parts.join(" ")} · Hopsync` : "Hopsync";
  }, [up, down]);
}

/** The stream is expected to be down for a moment at load and on a blip; say so only if it stays down. */
function ConnectionNotice() {
  const connected = useLive((s) => s.connected());
  const [show, setShow] = useState(false);
  useEffect(() => {
    if (connected) {
      setShow(false);
      return;
    }
    const t = setTimeout(() => setShow(true), 3000);
    return () => clearTimeout(t);
  }, [connected]);
  if (!show) return null;
  return (
    <p role="status" className="flex items-center gap-2 border-b border-line bg-differs px-4 py-1.5 text-warn">
      <WifiOff aria-hidden className="size-3.5" />
      Lost contact with the daemon. Reconnecting. Live numbers are paused.
    </p>
  );
}

export function Shell({ onLoggedOut }: { onLoggedOut: () => void }) {
  const hash = useHash();
  const [, view = "transfers", arg] = (hash || "#/transfers").split("/");
  const { theme, toggle } = useTheme();
  useTitleSpeed();
  useJobSync();

  const active = view === "job" ? "transfers" : NAV.some((n) => n.id === view) ? view : "transfers";
  const logout = async () => {
    try {
      await api("POST", "/api/logout", {});
    } catch (e) {
      toast(message(e));
    }
    onLoggedOut();
  };

  const tools = (
    <>
      <button type="button" className="btn btn-ghost btn-icon" aria-label={theme === "light" ? "Use the dark theme" : "Use the light theme"} title={theme === "light" ? "Dark theme" : "Light theme"} onClick={toggle}>
        {theme === "light" ? <Moon aria-hidden className="size-4" /> : <Sun aria-hidden className="size-4" />}
      </button>
      <button type="button" className="btn btn-ghost btn-icon" aria-label="Log out" title="Log out" onClick={() => void logout()}>
        <LogOut aria-hidden className="size-4" />
      </button>
    </>
  );

  return (
    <div className="flex h-dvh flex-col md:flex-row">
      <aside className="hidden w-48 shrink-0 flex-col border-r border-line bg-surface md:flex">
        <a href="#/transfers" className="flex items-center gap-2 px-4 py-3.5 text-sm font-semibold">
          <img src="/icon.svg" alt="" className="size-6" />
          Hopsync
        </a>
        <nav aria-label="Main" className="flex flex-col gap-0.5 px-2">
          {NAV.map((n) => (
            <a
              key={n.id}
              href={`#/${n.id}`}
              aria-current={active === n.id ? "page" : undefined}
              className={cn(
                "flex items-center gap-2.5 rounded-md px-2.5 py-1.5 font-medium text-muted transition-colors duration-100 hover:bg-hover hover:text-text",
                active === n.id && "bg-hover text-text",
              )}
            >
              <n.icon aria-hidden className="size-4" />
              {n.label}
            </a>
          ))}
        </nav>
        <div className="mt-auto flex gap-1 p-2">{tools}</div>
      </aside>

      <header className="flex items-center justify-between border-b border-line bg-surface px-3 py-1.5 md:hidden">
        <a href="#/transfers" className="flex items-center gap-2 font-semibold">
          <img src="/icon.svg" alt="" className="size-6" />
          Hopsync
        </a>
        <div className="flex gap-1">{tools}</div>
      </header>

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <ConnectionNotice />
        <main className="min-h-0 flex-1 overflow-auto p-3 md:p-4">
          {active === "transfers" && view !== "job" && <TransfersPage />}
          {view === "job" && <JobPage key={arg} id={Number(arg)} />}
          {active === "servers" && <ServersPage />}
          {active === "settings" && <SettingsPage />}
        </main>
      </div>

      <nav aria-label="Main" className="flex border-t border-line bg-surface pb-[env(safe-area-inset-bottom)] md:hidden">
        {NAV.map((n) => (
          <a
            key={n.id}
            href={`#/${n.id}`}
            aria-current={active === n.id ? "page" : undefined}
            className={cn("flex min-h-12 flex-1 flex-col items-center justify-center gap-0.5 text-[11px] text-muted", active === n.id && "text-text")}
          >
            <n.icon aria-hidden className="size-4" />
            {n.label}
          </a>
        ))}
      </nav>
    </div>
  );
}
