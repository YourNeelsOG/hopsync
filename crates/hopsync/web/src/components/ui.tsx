// Shared pieces: dialog, form field, state badge, toasts, the animated number and virtual rows.

import { useVirtualizer } from "@tanstack/react-virtual";
import { CircleAlert, CircleCheck } from "lucide-react";
import { AnimatePresence, m, useMotionValueEvent, useReducedMotion, useSpring } from "motion/react";
import { cloneElement, type ReactElement, type ReactNode, useEffect, useId, useRef, useState } from "react";
import type { JobState } from "../lib/api";
import { cn, useToasts } from "../lib/util";

export function Dialog({
  title,
  wide,
  onClose,
  children,
}: {
  title: string;
  wide?: boolean;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.showModal();
    // Closing on unmount is what makes the browser put focus back on the button that opened it.
    return () => el.close();
  }, []);
  return (
    <dialog
      ref={ref}
      aria-labelledby={titleId}
      className={cn("modal", wide && "modal-wide")}
      onClose={onClose}
      onClick={(e) => {
        // A click on the backdrop reports the dialog itself as the target.
        if (e.target === ref.current) ref.current.close();
      }}
    >
      <h2 id={titleId} className="mb-3 break-words">
        {title}
      </h2>
      {children}
    </dialog>
  );
}

export function DialogButtons({ children }: { children: ReactNode }) {
  return <div className="mt-4 flex flex-wrap justify-end gap-2">{children}</div>;
}

/** A label and its one control. The hint describes the control without becoming part of its name. */
export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactElement<{ "aria-describedby"?: string }> }) {
  const hintId = useId();
  return (
    <div className="my-2.5 flex flex-col gap-1">
      <label className="flex flex-col gap-1">
        <span className="text-xs font-medium">{label}</span>
        {hint ? cloneElement(children, { "aria-describedby": hintId }) : children}
      </label>
      {hint && (
        <small id={hintId} className="text-xs text-muted">
          {hint}
        </small>
      )}
    </div>
  );
}

export function FormError({ text }: { text: string }) {
  if (!text) return null;
  return (
    <p role="alert" className="my-2 text-bad">
      {text}
    </p>
  );
}

const STATE_STYLE: Record<JobState, string> = {
  queued: "text-muted border-line-strong",
  running: "text-accent border-accent",
  paused: "text-warn border-warn",
  done: "text-ok border-ok",
  failed: "text-bad border-bad",
  cancelled: "text-muted border-line-strong",
};

const STATE_LABEL: Record<JobState, string> = {
  queued: "Queued",
  running: "Running",
  paused: "Paused",
  done: "Done",
  failed: "Failed",
  cancelled: "Cancelled",
};

export function StateBadge({ state }: { state: JobState }) {
  return (
    <span className={cn("inline-block rounded-full border px-2 text-[11px] font-medium whitespace-nowrap", STATE_STYLE[state])}>
      {STATE_LABEL[state]}
    </span>
  );
}

export function Toasts() {
  const toasts = useToasts();
  return (
    <div aria-live="polite" className="fixed right-4 bottom-[calc(64px+env(safe-area-inset-bottom))] z-30 md:bottom-4 flex max-w-[min(420px,calc(100vw-32px))] flex-col gap-2">
      <AnimatePresence initial={false}>
        {toasts.map((t) => (
          <m.div
            key={t.id}
            role={t.kind === "error" ? "alert" : "status"}
            initial={{ opacity: 0, y: 8 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="card flex items-start gap-2 bg-raised px-3 py-2 break-words shadow-lg"
          >
            {t.kind === "error" ? (
              <CircleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-bad" />
            ) : (
              <CircleCheck aria-hidden className="mt-0.5 size-4 shrink-0 text-ok" />
            )}
            <span className="min-w-0">{t.text}</span>
          </m.div>
        ))}
      </AnimatePresence>
    </div>
  );
}

/**
 * A number that counts to its new value instead of jumping. The spring writes straight to the text
 * node, so a counter running at the display's frame rate causes no React renders. React only ever
 * sees the first value.
 */
export function AnimatedNumber({ value, format, className }: { value: number; format: (n: number) => string; className?: string }) {
  const reduced = useReducedMotion();
  const ref = useRef<HTMLSpanElement>(null);
  const spring = useSpring(value, { stiffness: 170, damping: 30, mass: 1 });
  const [first] = useState(() => format(value));
  useEffect(() => {
    if (reduced) spring.jump(value);
    else spring.set(value);
  }, [value, reduced, spring]);
  useMotionValueEvent(spring, "change", (v) => {
    if (ref.current) ref.current.textContent = format(Math.max(0, v));
  });
  return (
    <span ref={ref} className={cn("tabular-nums", className)}>
      {first}
    </span>
  );
}

/**
 * Renders only the rows in view. `headerHeight` is the height of a sticky header placed before
 * these rows inside the same scroller `scrollRef`.
 */
export function VirtualRows({
  count,
  rowHeight,
  scrollRef,
  headerHeight = 0,
  renderRow,
}: {
  count: number;
  rowHeight: number;
  scrollRef: React.RefObject<HTMLElement | null>;
  headerHeight?: number;
  renderRow: (index: number) => ReactNode;
}) {
  const virtual = useVirtualizer({
    count,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: 12,
    scrollMargin: headerHeight,
    // Rendered once before the scroller has a size (and in tests): show a screenful.
    initialRect: { width: 800, height: 600 },
  });
  return (
    <div role="rowgroup" className="relative w-full" style={{ height: virtual.getTotalSize() }}>
      {virtual.getVirtualItems().map((item) => (
        <div
          key={item.key}
          className="absolute top-0 left-0 w-full"
          style={{ height: item.size, transform: `translateY(${item.start - headerHeight}px)` }}
        >
          {renderRow(item.index)}
        </div>
      ))}
    </div>
  );
}

/** Row height for the current pointer: must match `.row-height` in index.css. */
export function useRowHeight(): number {
  const [coarse] = useState(() => window.matchMedia?.("(pointer: coarse)").matches ?? false);
  return coarse ? 44 : 30;
}
