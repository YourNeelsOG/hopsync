// Transfer defaults, the admin password, and the audit log.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { Field } from "../components/ui";
import { api, type AuditRow, type Existing, message, type Settings } from "../lib/api";
import { parseSize, sizeInput, stamp } from "../lib/format";
import { useSettings } from "../lib/hooks";
import { toast } from "../lib/util";
import { POLICIES } from "./TransferDialog";

function Defaults({ settings }: { settings: Settings }) {
  const client = useQueryClient();
  const [existing, setExisting] = useState<Existing>(settings.default_existing);
  const [verify, setVerify] = useState(settings.default_verify);
  const [minFree, setMinFree] = useState(sizeInput(settings.local_min_free));

  const submit = async (ev: FormEvent) => {
    ev.preventDefault();
    const n = parseSize(minFree);
    if (n === null) {
      toast("Minimum free space: use a size like 10G or 0.");
      return;
    }
    try {
      const saved = await api<Settings>("PUT", "/api/settings", { default_existing: existing, default_verify: verify, local_min_free: n });
      client.setQueryData(["settings"], saved);
      toast("Saved.", "ok");
    } catch (e) {
      toast(message(e));
    }
  };

  return (
    <form onSubmit={(e) => void submit(e)} className="card p-4">
      <h2>Defaults for new transfers</h2>
      <Field label="If a file exists">
        <select name="default_existing" className="input" value={existing} onChange={(e) => setExisting(POLICIES.find((p) => p.value === e.target.value)?.value ?? "skip")}>
          {POLICIES.map((p) => (
            <option key={p.value} value={p.value}>
              {p.label}
            </option>
          ))}
        </select>
      </Field>
      <label className="my-2.5 flex items-center gap-2">
        <input type="checkbox" name="default_verify" checked={verify} onChange={(e) => setVerify(e.target.checked)} />
        Verify after copying
      </label>
      <Field label="Minimum free space on this box" hint="Downloads that would leave less than this free are refused, for example 20G.">
        <input className="input" name="local_min_free" value={minFree} onChange={(e) => setMinFree(e.target.value)} />
      </Field>
      <div className="mt-3 flex justify-end">
        <button type="submit" className="btn btn-primary">
          Save
        </button>
      </div>
    </form>
  );
}

function Password() {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [again, setAgain] = useState("");

  const submit = async (ev: FormEvent) => {
    ev.preventDefault();
    if (next !== again) {
      toast("The new passwords differ.");
      return;
    }
    try {
      await api("POST", "/api/password", { current, new: next });
      toast("Password changed. Other sessions were logged out.", "ok");
      setCurrent("");
      setNext("");
      setAgain("");
    } catch (e) {
      toast(message(e));
    }
  };

  return (
    <form onSubmit={(e) => void submit(e)} className="card p-4">
      <h2>Admin password</h2>
      <Field label="Current password">
        <input className="input" type="password" autoComplete="current-password" required value={current} onChange={(e) => setCurrent(e.target.value)} />
      </Field>
      <Field label="New password" hint="At least 12 characters. Changing it logs out every other session.">
        <input className="input" type="password" autoComplete="new-password" required minLength={12} value={next} onChange={(e) => setNext(e.target.value)} />
      </Field>
      <Field label="Repeat new password">
        <input className="input" type="password" autoComplete="new-password" required value={again} onChange={(e) => setAgain(e.target.value)} />
      </Field>
      <div className="mt-3 flex justify-end">
        <button type="submit" className="btn btn-primary">
          Change password
        </button>
      </div>
    </form>
  );
}

function Audit() {
  const { data: rows, isPending } = useQuery({ queryKey: ["audit"], queryFn: () => api<AuditRow[]>("GET", "/api/audit?limit=200") });
  return (
    <div className="card p-4">
      <h2 className="mb-2">Audit log</h2>
      {isPending && <div aria-busy="true" className="skeleton h-16" />}
      {rows?.length === 0 && <p className="text-muted">Empty.</p>}
      {rows && rows.length > 0 && (
        <div className="scroller max-h-[50dvh]">
          <table className="w-full text-xs">
            <thead className="sr-only">
              <tr>
                <th>Time</th>
                <th>Event</th>
                <th>Detail</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r, i) => (
                <tr key={`${r.at}-${i}`} className="border-b border-line last:border-0">
                  <td className="py-1 pr-3 align-top whitespace-nowrap text-muted tabular-nums">{stamp(r.at)}</td>
                  <td className="py-1 pr-3 align-top font-medium whitespace-nowrap">{r.event}</td>
                  <td className="py-1 break-all">{r.detail}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

export function SettingsPage() {
  const { data: settings } = useSettings();
  return (
    <div className="mx-auto flex max-w-5xl flex-col gap-3">
      <h1>Settings</h1>
      <div className="grid items-start gap-3 md:grid-cols-2">
        {settings ? <Defaults settings={settings} /> : <div aria-busy="true" className="skeleton h-64" />}
        <Password />
      </div>
      <Audit />
    </div>
  );
}
