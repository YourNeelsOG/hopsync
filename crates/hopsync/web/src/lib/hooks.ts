// Shared data hooks over the API.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { api, type Job, message, type Server, type Settings } from "./api";
import { useLive } from "./live";
import { toast } from "./util";

export const useServers = () => useQuery({ queryKey: ["servers"], queryFn: () => api<Server[]>("GET", "/api/servers") });

export const useSettings = () => useQuery({ queryKey: ["settings"], queryFn: () => api<Settings>("GET", "/api/settings") });

export const useJobs = () => useQuery({ queryKey: ["jobs"], queryFn: () => api<Job[]>("GET", "/api/jobs") });

const FINAL = new Set<Job["state"]>(["done", "failed", "cancelled"]);

/**
 * Fetches the job list only when the daemon's change counter moves, not on every progress update,
 * and reloads the folders when a job finishes so its result shows without a manual refresh.
 */
export function useJobSync() {
  const client = useQueryClient();
  const changes = useLive((s) => s.snapshot().changes);
  const { data: jobs } = useJobs();
  const before = useRef(new Map<number, Job["state"]>());

  useEffect(() => {
    if (changes >= 0) void client.invalidateQueries({ queryKey: ["jobs"] });
  }, [changes, client]);

  useEffect(() => {
    if (!jobs) return;
    const finished = jobs.some((j) => FINAL.has(j.state) && before.current.has(j.id) && before.current.get(j.id) !== j.state);
    before.current = new Map(jobs.map((j) => [j.id, j.state]));
    if (finished) void client.invalidateQueries({ queryKey: ["list"] });
  }, [jobs, client]);
}

/** A POST or DELETE on a job. Its failure is a toast; the job list follows from the event stream. */
export function useJobAction() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ method = "POST", path, body = {} }: { method?: string; path: string; body?: unknown }) =>
      api(method, path, method === "DELETE" ? undefined : body),
    onError: (e) => toast(message(e)),
    onSettled: () => client.invalidateQueries({ queryKey: ["jobs"] }),
  });
}
