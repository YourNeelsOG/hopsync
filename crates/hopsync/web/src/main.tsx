// Entry point. Fonts and icons are bundled, so nothing loads from another site.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { domAnimation, LazyMotion } from "motion/react";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";
import { prefs } from "./lib/util";

const theme = prefs.get<string>("theme", "dark");
document.documentElement.dataset.theme = theme === "light" ? "light" : "dark";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: false,
      refetchOnWindowFocus: false,
      staleTime: 0,
    },
  },
});

const root = document.getElementById("app");
if (root) {
  createRoot(root).render(
    <StrictMode>
      <QueryClientProvider client={queryClient}>
        <LazyMotion features={domAnimation} strict>
          <App />
        </LazyMotion>
      </QueryClientProvider>
    </StrictMode>,
  );
}
