// Small shared helpers: class names, per-viewer preferences, the hash route, toasts.

import { clsx, type ClassValue } from "clsx";
import { useSyncExternalStore } from "react";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}

/** Per-viewer conveniences only (last folder, tab, theme). Never anything that matters. */
export const prefs = {
  get<T>(key: string, fallback: T): T {
    try {
      const v = localStorage.getItem(`hopsync.${key}`);
      return v === null ? fallback : (JSON.parse(v) as T);
    } catch {
      return fallback;
    }
  },
  set(key: string, value: unknown) {
    try {
      localStorage.setItem(`hopsync.${key}`, JSON.stringify(value));
    } catch {
      // Storage blocked or full: the UI works without it.
    }
  },
};

export function useHash(): string {
  return useSyncExternalStore(
    (fn) => {
      window.addEventListener("hashchange", fn);
      return () => window.removeEventListener("hashchange", fn);
    },
    () => location.hash,
  );
}

export type ToastKind = "error" | "ok";
export interface Toast {
  id: number;
  text: string;
  kind: ToastKind;
}

let toasts: readonly Toast[] = [];
let nextToast = 1;
const toastListeners = new Set<() => void>();

function setToasts(next: readonly Toast[]) {
  toasts = next;
  for (const fn of toastListeners) fn();
}

export function toast(text: string, kind: ToastKind = "error") {
  const id = nextToast++;
  setToasts([...toasts, { id, text, kind }]);
  setTimeout(() => setToasts(toasts.filter((t) => t.id !== id)), kind === "error" ? 8000 : 3500);
}

export function useToasts(): readonly Toast[] {
  return useSyncExternalStore(
    (fn) => {
      toastListeners.add(fn);
      return () => toastListeners.delete(fn);
    },
    () => toasts,
  );
}
