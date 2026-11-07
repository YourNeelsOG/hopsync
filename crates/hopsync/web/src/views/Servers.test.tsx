// The Add server form's password choice: the password logs in once, and never reaches the server record.

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ServersPage } from "./Servers";

const saved = { id: 7, name: "box", host: "h.example", port: 22, user: "me", root: "/home/me", key_file: null, key_secret: 1, min_free: 0, blocked: null, created: 1 };
const secret = "correct horse battery";
type Call = { method: string; path: string; body: string };
let calls: Call[];
let installStatus: number;

function reply(status: number, body: unknown) {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

beforeEach(() => {
  // jsdom has no modal dialogs.
  HTMLDialogElement.prototype.showModal = function () {
    this.open = true;
  };
  HTMLDialogElement.prototype.close = function () {
    this.open = false;
  };
  calls = [];
  installStatus = 200;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input);
      const method = init?.method ?? "GET";
      calls.push({ method, path, body: typeof init?.body === "string" ? init.body : "" });
      if (method === "POST" && path === "/api/servers") return reply(201, saved);
      if (path === "/api/servers") return reply(200, []);
      if (path.endsWith("/probe")) return reply(200, { status: "unknown", algorithm: "ssh-ed25519", fingerprint: "SHA256:abc", pinned: [] });
      if (path.endsWith("/trust")) return reply(200, {});
      if (path.endsWith("/install-key")) return installStatus === 200 ? reply(200, {}) : reply(installStatus, { error: "wrong password" });
      if (path.endsWith("/public-key")) return reply(200, { public_key: "ssh-ed25519 AAAA hopsync-server-7" });
      return reply(404, {});
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function addWithPassword() {
  render(
    <QueryClientProvider client={new QueryClient()}>
      <ServersPage />
    </QueryClientProvider>,
  );
  fireEvent.click(screen.getByRole("button", { name: /Add server/ }));
  const type = (name: string, value: string) => {
    const el = document.querySelector<HTMLInputElement>(`input[name="${name}"]`);
    if (!el) throw new Error(`no field ${name}`);
    fireEvent.change(el, { target: { value } });
  };
  type("name", "box");
  type("user", "me");
  type("host", "h.example");
  type("root", "/home/me");
  type("ssh_password", secret);
  fireEvent.click(screen.getByRole("button", { name: "Add" }));
  fireEvent.click(await screen.findByRole("button", { name: "They match: trust" }));
}

describe("adding a server with a password", () => {
  it("is the choice offered first", () => {
    render(
      <QueryClientProvider client={new QueryClient()}>
        <ServersPage />
      </QueryClientProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: /Add server/ }));
    expect(screen.getByRole<HTMLInputElement>("radio", { name: /Password/ }).checked).toBe(true);
  });

  it("logs in once after the host key is trusted, and keeps the password out of the record", async () => {
    await addWithPassword();
    await waitFor(() => expect(calls.some((c) => c.path === "/api/servers/7/install-key")).toBe(true));

    const trust = calls.findIndex((c) => c.path.endsWith("/trust"));
    const install = calls.findIndex((c) => c.path.endsWith("/install-key"));
    expect(trust).toBeGreaterThanOrEqual(0);
    expect(trust).toBeLessThan(install);
    expect(JSON.parse(calls[install]?.body ?? "{}")).toEqual({ password: secret });

    const elsewhere = calls.filter((c) => !c.path.endsWith("/install-key"));
    expect(elsewhere.some((c) => c.body.includes(secret))).toBe(false);
    expect(JSON.parse(calls.find((c) => c.method === "POST" && c.path === "/api/servers")?.body ?? "{}").generate_key).toBe(true);
  });

  it("offers the other ways in and says why when the login fails", async () => {
    installStatus = 400;
    await addWithPassword();
    expect(await screen.findByText("wrong password")).toBeTruthy();
    expect(screen.getByLabelText("Public key")).toBeTruthy();
  });
});
