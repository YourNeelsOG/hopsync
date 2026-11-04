import { afterEach, describe, expect, it, vi } from "vitest";
import { api, ApiError, onAuthRequired } from "./api";

function respond(status: number, body: string) {
  vi.stubGlobal("fetch", vi.fn(async () => new Response(body, { status })));
}

afterEach(() => {
  vi.unstubAllGlobals();
  onAuthRequired(() => {});
});

describe("api", () => {
  it("sends JSON and returns the parsed answer", async () => {
    respond(200, '{"a":1}');
    await expect(api("POST", "/api/x", { b: 2 })).resolves.toEqual({ a: 1 });
    const [path, init] = vi.mocked(fetch).mock.calls[0] ?? [];
    expect(path).toBe("/api/x");
    expect(init?.body).toBe('{"b":2}');
    expect(new Headers(init?.headers).get("Content-Type")).toBe("application/json");
  });

  it("uses the daemon's error text", async () => {
    respond(400, '{"error":"bad path"}');
    const e = await api("GET", "/api/x").catch((err: unknown) => err);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 400, message: "bad path" });
  });

  it("reports an unreachable daemon in words", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => Promise.reject(new TypeError("failed"))));
    await expect(api("GET", "/api/x")).rejects.toMatchObject({
      status: 0,
      message: "Hopsync is not reachable. Is the daemon running?",
    });
  });

  it("asks for a login on a 401, except from the login call itself", async () => {
    const seen = vi.fn();
    onAuthRequired(seen);
    respond(401, '{"error":"log in"}');
    await api("GET", "/api/servers").catch(() => {});
    expect(seen).toHaveBeenCalledWith("login");
    seen.mockClear();
    await api("POST", "/api/login", { password: "x" }).catch(() => {});
    expect(seen).not.toHaveBeenCalled();
  });

  it("asks for setup when the daemon says so", async () => {
    const seen = vi.fn();
    onAuthRequired(seen);
    respond(403, '{"error":"setup is required"}');
    await api("GET", "/api/servers").catch(() => {});
    expect(seen).toHaveBeenCalledWith("setup");
  });
});
