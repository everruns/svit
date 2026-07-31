import { defineConfig } from "astro/config";

export default defineConfig({
  site: "https://svit.everruns.com",
  output: "static",
  build: {
    format: "directory",
  },
});
