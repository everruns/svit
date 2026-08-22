import { defineConfig } from "astro/config";
import icon from "astro-icon";
import tailwindcss from "@tailwindcss/vite";
import nimbus, { defineConfig as defineNimbusConfig } from "@cloudflare/nimbus-docs";
import { tableScroll } from "@cloudflare/nimbus-docs/markdown";
import type { HastPluginInput } from "@cloudflare/nimbus-docs/types";

const mermaidCodeBlocks: HastPluginInput = {
  name: "svit:mermaid-code-blocks",
  element: {
    filter: ["figure"],
    visit(node, context) {
      const language = node.properties?.dataNbLang ?? node.properties?.["data-nb-lang"];
      if (language === "mermaid") {
        context.setProperty(node, "dataMermaidDiagram", "");
      }
    },
  },
};

const nimbusConfig = defineNimbusConfig({
  site: "https://svit.everruns.com",
  title: "Svit",
  description: "A research-stage Rust runtime for durable agent state and reusable code.",
  locale: "en",
  homeLabel: "Svit",
  github: "https://github.com/everruns/svit",
  editPattern: null,
  sidebar: {
    items: [
      "overview",
      {
        label: "Concepts",
        collapsed: false,
        items: ["memory", "ports", "events"],
      },
      {
        label: "Architecture",
        collapsed: false,
        items: ["vision", "control-protocol"],
      },
      {
        label: "Project",
        collapsed: false,
        items: ["security", "changelog"],
      },
    ],
  },
  head: [
    { tag: "meta", attrs: { name: "theme-color", content: "#0a1636" } },
  ],
});
export default defineConfig({
  output: "static",
  markdown: {
    shikiConfig: {
      // Keep dual-theme token colors in the HTML when Nimbus' generated
      // class registry is unavailable during a static build.
      defaultColor: "light-dark()",
    },
  },
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
        hastPlugins: [tableScroll(), mermaidCodeBlocks],
      },
    }),
  ],
});
