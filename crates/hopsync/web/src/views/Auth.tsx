// First-run setup and login pages. The setup token never appears in a log; the page says where it is.

import { type FormEvent, type ReactNode, useState } from "react";
import { Field, FormError } from "../components/ui";
import { api, ApiError, message } from "../lib/api";

function Brand() {
  return (
    <div className="mb-5 flex items-center gap-2 text-base font-semibold">
      <img src="/icon.svg" alt="" className="size-7" />
      Hopsync
    </div>
  );
}

function AuthCard({
  title,
  children,
  submit,
  onSubmit,
}: {
  title: string;
  children: ReactNode;
  submit: string;
  onSubmit: (form: FormData) => Promise<void>;
}) {
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const handle = async (ev: FormEvent<HTMLFormElement>) => {
    ev.preventDefault();
    setError("");
    setBusy(true);
    try {
      await onSubmit(new FormData(ev.currentTarget));
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <main className="flex min-h-dvh items-center justify-center p-4">
      <form onSubmit={(e) => void handle(e)} className="card w-full max-w-sm p-6">
        <Brand />
        <h1 className="mb-2">{title}</h1>
        {children}
        <FormError text={error} />
        <button type="submit" className="btn btn-primary mt-3 w-full" disabled={busy}>
          {submit}
        </button>
      </form>
    </main>
  );
}

const text = (form: FormData, key: string) => {
  const v = form.get(key);
  return typeof v === "string" ? v : "";
};

export function SetupPage({ onDone }: { onDone: () => Promise<void> }) {
  return (
    <AuthCard
      title="Set the admin password"
      submit="Set password"
      onSubmit={async (form) => {
        if (text(form, "password") !== text(form, "again")) throw new Error("The passwords differ.");
        await api("POST", "/api/setup", { token: text(form, "token").trim(), password: text(form, "password") });
        await onDone();
      }}
    >
      <p className="text-muted">
        This is the first start. The daemon wrote a one-time setup token to the file <code>setup-token</code> in its data directory. Its log
        names the full path. Paste the token here and choose a password.
      </p>
      <Field label="Setup token">
        <input className="input font-mono" name="token" required autoComplete="off" spellCheck={false} autoFocus />
      </Field>
      <Field label="Password" hint="At least 12 characters.">
        <input className="input" name="password" type="password" required minLength={12} autoComplete="new-password" />
      </Field>
      <Field label="Repeat password">
        <input className="input" name="again" type="password" required autoComplete="new-password" />
      </Field>
    </AuthCard>
  );
}

export function LoginPage({ onDone }: { onDone: () => Promise<void> }) {
  return (
    <AuthCard
      title="Log in"
      submit="Log in"
      onSubmit={async (form) => {
        try {
          await api("POST", "/api/login", { password: text(form, "password") });
        } catch (e) {
          if (e instanceof ApiError && e.status === 401) throw new Error("Wrong password.");
          throw e;
        }
        await onDone();
      }}
    >
      <Field label="Password">
        <input className="input" name="password" type="password" required autoComplete="current-password" autoFocus />
      </Field>
    </AuthCard>
  );
}

export function UnreachablePage({ text: body }: { text: string }) {
  return (
    <main className="flex min-h-dvh items-center justify-center p-4">
      <div className="card w-full max-w-sm p-6">
        <Brand />
        <p role="alert" className="text-bad">
          {body}
        </p>
      </div>
    </main>
  );
}
