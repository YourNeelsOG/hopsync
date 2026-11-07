// Servers: add and edit, confirm host keys by fingerprint, install generated keys, remove.
// A host key is only ever pinned by an explicit click; a changed key also needs a ticked confirmation.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { KeyRound, Pencil, Plus, ShieldAlert, ShieldCheck, Trash2 } from "lucide-react";
import { type FormEvent, useEffect, useState } from "react";
import { Dialog, DialogButtons, Field, FormError } from "../components/ui";
import { api, message, type Probe, type Server, type ServerSpec } from "../lib/api";
import { parseSize, sizeInput } from "../lib/format";
import { useServers } from "../lib/hooks";
import { toast } from "../lib/util";

type Flow =
  | { kind: "form"; server: Server | null }
  // `password` lives here only until the one login it is for.
  | { kind: "trust"; server: Server; thenInstall: boolean; password?: string }
  | { kind: "connecting"; server: Server }
  | { kind: "install"; server: Server; error?: string }
  | { kind: "remove"; server: Server };

function useReloadServers() {
  const client = useQueryClient();
  return () => client.invalidateQueries({ queryKey: ["servers"] });
}

const str = (f: FormData, k: string) => {
  const v = f.get(k);
  return typeof v === "string" ? v.trim() : "";
};

function ServerForm({ server, onClose, onAdded }: { server: Server | null; onClose: () => void; onAdded: (s: Server, generated: boolean, password?: string) => void }) {
  const reload = useReloadServers();
  const editing = server !== null;
  const hasGeneratedKey = server?.key_secret != null;
  const [auth, setAuth] = useState<"password" | "generate" | "file">(editing ? "file" : "password");
  const [error, setError] = useState("");

  const submit = async (ev: FormEvent<HTMLFormElement>) => {
    ev.preventDefault();
    setError("");
    const f = new FormData(ev.currentTarget);
    const minFree = parseSize(str(f, "min_free") || "0");
    if (minFree === null) {
      setError("Minimum free space: use a size like 10G or 0.");
      return;
    }
    const spec: ServerSpec = {
      name: str(f, "name"),
      host: str(f, "host"),
      port: Number(str(f, "port") || 22),
      user: str(f, "user"),
      root: str(f, "root"),
      min_free: minFree,
    };
    // A password also gets a generated key: it logs in once to install it, and is never stored.
    const generate = !editing && auth !== "file";
    const password = !editing && auth === "password" ? String(f.get("ssh_password") ?? "") : undefined;
    if (!generate && !hasGeneratedKey) spec.key_file = str(f, "key_file");
    try {
      const saved = editing ? await api<Server>("PUT", `/api/servers/${server.id}`, spec) : await api<Server>("POST", "/api/servers", { ...spec, generate_key: generate });
      await reload();
      onClose();
      if (!editing) onAdded(saved, generate, password);
    } catch (e) {
      setError(message(e));
    }
  };

  return (
    <Dialog title={editing ? `Edit ${server.name}` : "Add a server"} onClose={onClose}>
      <form onSubmit={(e) => void submit(e)}>
        <div className="grid gap-x-3 sm:grid-cols-2">
          <Field label="Name">
            <input className="input" name="name" required maxLength={64} placeholder="seedbox" defaultValue={server?.name} />
          </Field>
          <Field label="User">
            <input className="input" name="user" required autoComplete="off" defaultValue={server?.user} />
          </Field>
          <Field label="Host">
            <input className="input" name="host" required placeholder="box.example.com" spellCheck={false} defaultValue={server?.host} />
          </Field>
          <Field label="Port">
            <input className="input" name="port" type="number" min={1} max={65535} defaultValue={server?.port ?? 22} />
          </Field>
        </div>
        <Field label="Root folder" hint="Absolute path on the server. Hopsync never reads or writes outside it.">
          <input className="input font-mono" name="root" required placeholder="/home/you" spellCheck={false} defaultValue={server?.root} />
        </Field>
        <Field label="Minimum free space" hint="Uploads that would leave less than this free are refused, for example 10G. This protects the box's own work.">
          <input className="input" name="min_free" defaultValue={server ? sizeInput(server.min_free) : "0"} />
        </Field>
        {hasGeneratedKey ? (
          <p className="text-muted">Authentication: a key Hopsync generated for this server.</p>
        ) : (
          <fieldset className="my-2.5 rounded-md border border-line p-3">
            <legend className="px-1 text-xs font-medium">Authentication</legend>
            {!editing && (
              <>
                <label className="my-1 flex items-center gap-2">
                  <input type="radio" name="auth" checked={auth === "password"} onChange={() => setAuth("password")} />
                  Password
                </label>
                {auth === "password" && (
                  <>
                    <input className="input mt-1" type="password" name="ssh_password" aria-label="SSH password" autoComplete="off" required />
                    <p className="mt-1 mb-2 text-xs text-muted">Used once to log in and set up a private key for this server. Your password is never stored.</p>
                  </>
                )}
                <label className="my-1 flex items-center gap-2">
                  <input type="radio" name="auth" checked={auth === "generate"} onChange={() => setAuth("generate")} />
                  Generate a key, and add it to the server myself
                </label>
              </>
            )}
            <label className="my-1 flex items-center gap-2">
              <input type="radio" name="auth" checked={auth === "file"} onChange={() => setAuth("file")} />
              Use a key file on this box
            </label>
            <input
              className="input mt-1 font-mono"
              name="key_file"
              aria-label="Key file"
              placeholder="/home/you/.ssh/id_ed25519"
              spellCheck={false}
              disabled={auth !== "file"}
              required={auth === "file"}
              defaultValue={server?.key_file ?? ""}
            />
          </fieldset>
        )}
        <FormError text={error} />
        <DialogButtons>
          <button type="button" className="btn btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary">
            {editing ? "Save" : "Add"}
          </button>
        </DialogButtons>
      </form>
    </Dialog>
  );
}

function Fingerprint({ value }: { value: string }) {
  return <code className="my-1.5 block rounded-md border border-line bg-bg px-2.5 py-2 break-all">{value}</code>;
}

function TrustDialog({ server, thenInstall, onClose, onInstall }: { server: Server; thenInstall: boolean; onClose: () => void; onInstall: (s: Server) => void }) {
  const reload = useReloadServers();
  const [ack, setAck] = useState(false);
  const probe = useQuery({
    queryKey: ["probe", server.id],
    queryFn: () => api<Probe>("POST", `/api/servers/${server.id}/probe`, {}),
    gcTime: 0,
  });
  const p = probe.data;
  const close = (
    <button type="button" className="btn btn-ghost" onClick={onClose}>
      Close
    </button>
  );

  useEffect(() => {
    if (p?.status === "trusted" && thenInstall) onInstall(server);
  }, [p?.status, thenInstall, onInstall, server]);

  const trust = async (replace: boolean) => {
    if (!p) return;
    try {
      await api("POST", `/api/servers/${server.id}/trust`, { fingerprint: p.fingerprint, replace });
      toast(`${server.name}: host key pinned.`, "ok");
      await reload();
      onClose();
      if (thenInstall) onInstall(server);
    } catch (e) {
      toast(message(e));
    }
  };

  return (
    <Dialog wide title={`Host key of ${server.name}`} onClose={onClose}>
      {probe.isPending && (
        <p aria-busy="true" className="text-muted">
          Asking {server.host}:{server.port} for its host key…
        </p>
      )}
      {probe.isError && (
        <>
          <p className="text-bad">{probe.error.message}</p>
          <DialogButtons>{close}</DialogButtons>
        </>
      )}
      {p?.status === "trusted" && (
        <>
          <p>The server offers the pinned {p.algorithm} key:</p>
          <Fingerprint value={p.fingerprint} />
          <p className="flex items-center gap-1.5 text-ok">
            <ShieldCheck aria-hidden className="size-4" /> It matches. Nothing to do.
          </p>
          <DialogButtons>{close}</DialogButtons>
        </>
      )}
      {p?.status === "unknown" && (
        <>
          <p>The server offers this {p.algorithm} key:</p>
          <Fingerprint value={p.fingerprint} />
          <p>
            Compare it with the fingerprint your provider shows, or with the output of <code>ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub</code> on the server. Trust it only if they
            match. Hopsync refuses any other key from then on.
          </p>
          <DialogButtons>
            {close}
            <button type="button" className="btn btn-primary" onClick={() => void trust(false)}>
              They match: trust
            </button>
          </DialogButtons>
        </>
      )}
      {p?.status === "changed" && (
        <>
          <p className="flex items-start gap-1.5 text-bad">
            <ShieldAlert aria-hidden className="mt-0.5 size-4 shrink-0" />
            The server now offers a different host key than the one pinned. This happens when a server is reinstalled, and also when someone is intercepting the connection.
          </p>
          <p className="mt-2">Pinned:</p>
          {p.pinned.map((k) => (
            <Fingerprint key={k} value={k} />
          ))}
          <p className="mt-2">Offered now ({p.algorithm}):</p>
          <Fingerprint value={p.fingerprint} />
          <label className="mt-2 flex items-start gap-2">
            <input type="checkbox" className="mt-0.5" checked={ack} onChange={(e) => setAck(e.target.checked)} />
            I confirmed with the provider that the server's key changed on purpose
          </label>
          <DialogButtons>
            {close}
            <button type="button" className="btn btn-danger" disabled={!ack} onClick={() => void trust(true)}>
              Replace the pinned key
            </button>
          </DialogButtons>
        </>
      )}
    </Dialog>
  );
}

function InstallDialog({ server, firstError, onClose }: { server: Server; firstError?: string; onClose: () => void }) {
  const pub = useQuery({
    queryKey: ["public-key", server.id],
    queryFn: () => api<{ public_key: string | null }>("GET", `/api/servers/${server.id}/public-key`),
  });
  const [password, setPassword] = useState("");
  const [keyFile, setKeyFile] = useState("");
  const [target, setTarget] = useState("");
  const [error, setError] = useState(firstError ?? "");
  const [busy, setBusy] = useState(false);

  const submit = async (ev: FormEvent) => {
    ev.preventDefault();
    setError("");
    const body: { password?: string; key_file?: string; authorized_keys?: string } = {};
    if (password) body.password = password;
    else if (keyFile.trim()) body.key_file = keyFile.trim();
    else {
      setError("Enter the account password, or a key file the server already accepts.");
      return;
    }
    if (target.trim()) body.authorized_keys = target.trim();
    setBusy(true);
    try {
      await api("POST", `/api/servers/${server.id}/install-key`, body);
      toast(`${server.name}: key installed and working.`, "ok");
      onClose();
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog wide title={`Install the key for ${server.name}`} onClose={onClose}>
      <form onSubmit={(e) => void submit(e)}>
        <p>Hopsync made a key just for this server. It needs to be in the account's authorized_keys file. Either add this line yourself:</p>
        <textarea readOnly rows={3} aria-label="Public key" className="input my-2 font-mono text-xs" value={pub.data?.public_key ?? ""} />
        <p>Or let Hopsync add it now, logging in once with the account password. Hopsync uses the password for that one login and never stores it.</p>
        <Field label="Account password">
          <input className="input" type="password" name="password" autoComplete="off" value={password} onChange={(e) => setPassword(e.target.value)} />
        </Field>
        <Field label="Or a key file on this box">
          <input className="input font-mono" name="key_file" placeholder="/home/you/.ssh/id_ed25519" spellCheck={false} value={keyFile} onChange={(e) => setKeyFile(e.target.value)} />
        </Field>
        <details className="my-2">
          <summary className="cursor-pointer text-muted">Advanced</summary>
          <Field label="authorized_keys file" hint="Relative to the login folder, or absolute. Must be named authorized_keys or authorized_keys2.">
            <input className="input font-mono" name="authorized_keys" placeholder=".ssh/authorized_keys" spellCheck={false} value={target} onChange={(e) => setTarget(e.target.value)} />
          </Field>
        </details>
        <FormError text={error} />
        <DialogButtons>
          <button type="button" className="btn btn-ghost" onClick={onClose}>
            Later
          </button>
          <button type="submit" className="btn btn-primary" disabled={busy}>
            Install
          </button>
        </DialogButtons>
      </form>
    </Dialog>
  );
}

function RemoveDialog({ server, onClose }: { server: Server; onClose: () => void }) {
  const reload = useReloadServers();
  return (
    <Dialog title={`Remove ${server.name}?`} onClose={onClose}>
      <p>Its finished jobs and its generated key are removed too. This cannot be undone.</p>
      <DialogButtons>
        <button type="button" className="btn btn-ghost" onClick={onClose}>
          Keep
        </button>
        <button
          type="button"
          className="btn btn-danger"
          onClick={() =>
            void (async () => {
              try {
                await api("DELETE", `/api/servers/${server.id}`);
                toast(`Removed ${server.name}.`, "ok");
                await reload();
              } catch (e) {
                toast(message(e));
              }
              onClose();
            })()
          }
        >
          Remove
        </button>
      </DialogButtons>
    </Dialog>
  );
}

export function ServersPage() {
  const { data: servers, isPending } = useServers();
  const [flow, setFlow] = useState<Flow | null>(null);
  const close = () => setFlow(null);

  // Once the host key is trusted, so the password only ever goes to the pinned server.
  const connect = async (server: Server, password?: string) => {
    if (!password) {
      setFlow({ kind: "install", server });
      return;
    }
    setFlow({ kind: "connecting", server });
    try {
      await api("POST", `/api/servers/${server.id}/install-key`, { password });
      toast(`${server.name}: connected.`, "ok");
      setFlow(null);
    } catch (e) {
      setFlow({ kind: "install", server, error: message(e) });
    }
  };

  return (
    <div className="mx-auto flex max-w-5xl flex-col gap-3">
      <div className="flex items-center justify-between">
        <h1>Servers</h1>
        <button type="button" className="btn btn-primary" onClick={() => setFlow({ kind: "form", server: null })}>
          <Plus aria-hidden className="size-3.5" /> Add server
        </button>
      </div>
      {isPending && <div aria-busy="true" className="skeleton h-20" />}
      {servers?.length === 0 && (
        <div className="card p-6 text-center">
          <p className="text-muted">No servers yet. Any SSH account works: the other side needs only OpenSSH, not Hopsync.</p>
        </div>
      )}
      {servers && servers.length > 0 && (
        <ul className="card divide-y divide-line">
          {servers.map((s) => (
            <li key={s.id} className="flex flex-wrap items-center gap-x-6 gap-y-2 p-3">
              <div className="min-w-40 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <strong className="break-all">{s.name}</strong>
                  {s.blocked && (
                    <span title={s.blocked} className="rounded-full border border-bad px-2 text-[11px] font-medium text-bad">
                      Blocked
                    </span>
                  )}
                </div>
                <div className="font-mono text-xs break-all text-muted">
                  {s.user}@{s.host}:{s.port}
                </div>
              </div>
              <dl className="grid min-w-48 flex-[2] grid-cols-[max-content_minmax(0,1fr)] gap-x-3 text-xs">
                <dt className="text-muted">Root</dt>
                <dd className="font-mono break-all">{s.root}</dd>
                <dt className="text-muted">Key</dt>
                <dd className="font-mono break-all">{s.key_secret != null ? "generated key" : s.key_file}</dd>
              </dl>
              <div className="flex flex-wrap gap-1">
                <button type="button" className={s.blocked ? "btn btn-warn btn-sm" : "btn btn-sm"} onClick={() => setFlow({ kind: "trust", server: s, thenInstall: false })}>
                  {s.blocked ? <ShieldAlert aria-hidden className="size-3.5" /> : <ShieldCheck aria-hidden className="size-3.5" />}
                  {s.blocked ? "Review key" : "Check key"}
                </button>
                {s.key_secret != null && (
                  <button type="button" className="btn btn-sm" onClick={() => setFlow({ kind: "install", server: s })}>
                    <KeyRound aria-hidden className="size-3.5" /> Install key
                  </button>
                )}
                <button type="button" className="btn btn-ghost btn-sm" onClick={() => setFlow({ kind: "form", server: s })}>
                  <Pencil aria-hidden className="size-3.5" /> Edit
                </button>
                <button type="button" className="btn btn-ghost btn-sm text-bad hover:text-bad" onClick={() => setFlow({ kind: "remove", server: s })}>
                  <Trash2 aria-hidden className="size-3.5" /> Remove
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
      {flow?.kind === "form" && <ServerForm server={flow.server} onClose={close} onAdded={(server, generated, password) => setFlow({ kind: "trust", server, thenInstall: generated, password })} />}
      {flow?.kind === "trust" && <TrustDialog key={flow.server.id} server={flow.server} thenInstall={flow.thenInstall} onClose={close} onInstall={(server) => void connect(server, flow.password)} />}
      {flow?.kind === "connecting" && (
        <Dialog title={`Connecting to ${flow.server.name}`} onClose={close}>
          <p aria-busy="true" className="text-muted">
            Logging in and setting up a private key…
          </p>
        </Dialog>
      )}
      {flow?.kind === "install" && <InstallDialog key={flow.error ?? ""} server={flow.server} firstError={flow.error} onClose={close} />}
      {flow?.kind === "remove" && <RemoveDialog server={flow.server} onClose={close} />}
    </div>
  );
}
