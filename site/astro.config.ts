import { defineConfig } from "astro/config";
import icon from "astro-icon";
import tailwindcss from "@tailwindcss/vite";
import nimbus, { defineConfig as defineNimbusConfig } from "@cloudflare/nimbus-docs";
import { tableScroll } from "@cloudflare/nimbus-docs/markdown";

const nimbusConfig = defineNimbusConfig({
  site: "https://svit.everruns.com",
  title: "Svit",
  description: "A research-stage Rust runtime for durable agent state and reusable code.",
  locale: "en",
  homeLabel: "Svit",
  github: "https://github.com/everruns/svit",
  editPattern: null,
  head: [
    { tag: "meta", attrs: { name: "theme-color", content: "#0a1636" } },
  ],
});
export default defineConfig({
  output: "static",
  vite: {
    plugins: [tailwindcss()],
  },
  prefetch: {
    prefetchAll: true,
    defaultStrategy: "hover",
  },
  integrations: [
    icon(),
    nimbus(nimbusConfig, {
      rules: {
        "nimbus/frontmatter-shape": "error",
        "nimbus/internal-link": "error",
      },
      markdown: {
        hastPlugins: [tableScroll()],
      },
    }),
  ],
});
