import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed port in dev and serves the built files from `dist`.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1430, strictPort: true, host: "127.0.0.1" },
  preview: { port: 1431, strictPort: true, host: "127.0.0.1" },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 800,
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
