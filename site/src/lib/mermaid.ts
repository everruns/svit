type MermaidApi = typeof import("mermaid")["default"];

const diagramSelector = "[data-mermaid-diagram]";
let mermaidPromise: Promise<MermaidApi> | undefined;
let renderSequence = 0;
let renderQueue = Promise.resolve();

function currentTheme(): "dark" | "default" {
  return document.documentElement.getAttribute("data-mode") === "dark"
    ? "dark"
    : "default";
}

function themeVariables(theme: "dark" | "default") {
  if (theme === "dark") {
    return {
      background: "#0f0f0f",
      primaryColor: "#172033",
      primaryTextColor: "#f1f5f9",
      primaryBorderColor: "#526786",
      secondaryColor: "#111827",
      secondaryTextColor: "#e2e8f0",
      secondaryBorderColor: "#3b4b66",
      tertiaryColor: "#0b1220",
      tertiaryTextColor: "#e2e8f0",
      tertiaryBorderColor: "#334155",
      lineColor: "#94a3b8",
      textColor: "#e2e8f0",
      mainBkg: "#172033",
      nodeBorder: "#526786",
      clusterBkg: "#0b1220",
      clusterBorder: "#334155",
      edgeLabelBackground: "#0f0f0f",
      actorBkg: "#172033",
      actorBorder: "#526786",
      actorTextColor: "#f1f5f9",
      signalColor: "#94a3b8",
      signalTextColor: "#e2e8f0",
      labelBoxBkgColor: "#111827",
      labelBoxBorderColor: "#3b4b66",
      labelTextColor: "#e2e8f0",
      loopTextColor: "#e2e8f0",
      noteBkgColor: "#172033",
      noteBorderColor: "#526786",
      noteTextColor: "#f1f5f9",
    };
  }

  return {
    background: "#ffffff",
    primaryColor: "#eef3ff",
    primaryTextColor: "#172033",
    primaryBorderColor: "#93a4c7",
    secondaryColor: "#f8fafc",
    secondaryTextColor: "#253149",
    secondaryBorderColor: "#cbd5e1",
    tertiaryColor: "#f1f5f9",
    tertiaryTextColor: "#253149",
    tertiaryBorderColor: "#cbd5e1",
    lineColor: "#64748b",
    textColor: "#253149",
    mainBkg: "#eef3ff",
    nodeBorder: "#93a4c7",
    clusterBkg: "#f8fafc",
    clusterBorder: "#d6deea",
    edgeLabelBackground: "#ffffff",
    actorBkg: "#eef3ff",
    actorBorder: "#93a4c7",
    actorTextColor: "#172033",
    signalColor: "#64748b",
    signalTextColor: "#334155",
    labelBoxBkgColor: "#f8fafc",
    labelBoxBorderColor: "#cbd5e1",
    labelTextColor: "#334155",
    loopTextColor: "#334155",
    noteBkgColor: "#eef3ff",
    noteBorderColor: "#93a4c7",
    noteTextColor: "#172033",
  };
}

function semanticThemeCss(theme: "dark" | "default"): string {
  const colors =
    theme === "dark"
      ? {
          external: ["#151a22", "#64748b", "#cbd5e1"],
          runtime: ["#17233b", "#6682b6", "#f1f5f9"],
          process: ["#27416f", "#7698d2", "#ffffff"],
          state: ["#142b24", "#4f9579", "#d1fae5"],
          artifact: ["#2a2340", "#8b75ba", "#ede9fe"],
        }
      : {
          external: ["#f8fafc", "#94a3b8", "#475569"],
          runtime: ["#eaf2ff", "#7790bf", "#172033"],
          process: ["#20355f", "#20355f", "#ffffff"],
          state: ["#ecf8f3", "#71a991", "#183b2a"],
          artifact: ["#f3efff", "#9b88c7", "#34264f"],
        };

  return Object.entries(colors)
    .map(([name, [fill, stroke, text]]) => {
      const dash = name === "external" ? "stroke-dasharray: 6 4 !important;" : "";
      return `
        .node.${name} > .label-container {
          fill: ${fill} !important;
          stroke: ${stroke} !important;
          ${dash}
        }
        .node.${name} .nodeLabel,
        .node.${name} .label {
          color: ${text} !important;
        }
      `;
    })
    .join("\n");
}

function loadMermaid(): Promise<MermaidApi> {
  mermaidPromise ??= import("mermaid").then(({ default: mermaid }) => mermaid);
  return mermaidPromise;
}

async function renderDiagrams(): Promise<void> {
  const figures = Array.from(
    document.querySelectorAll<HTMLElement>(diagramSelector),
  );
  if (figures.length === 0) return;

  const mermaid = await loadMermaid();
  const theme = currentTheme();
  mermaid.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    suppressErrorRendering: true,
    theme: "base",
    look: "neo",
    themeVariables: themeVariables(theme),
    themeCSS: semanticThemeCss(theme),
    fontFamily: getComputedStyle(document.documentElement)
      .getPropertyValue("--nb-font-sans")
      .trim(),
    flowchart: {
      curve: "rounded",
      diagramPadding: 12,
      nodeSpacing: 28,
      rankSpacing: 44,
    },
    sequence: {
      actorMargin: 24,
      actorFontSize: 13,
      boxMargin: 8,
      diagramMarginX: 12,
      diagramMarginY: 12,
      messageFontSize: 13,
      messageMargin: 28,
      mirrorActors: false,
      width: 96,
      wrap: true,
      wrapPadding: 8,
    },
  });

  const sequence = ++renderSequence;
  for (const [index, figure] of figures.entries()) {
    const source = figure.querySelector("pre code")?.textContent?.trim();
    if (!source) continue;

    try {
      const { svg, bindFunctions } = await mermaid.render(
        `svit-mermaid-${sequence}-${index}`,
        source,
      );
      let canvas = figure.querySelector<HTMLElement>("[data-mermaid-canvas]");
      if (!canvas) {
        canvas = document.createElement("div");
        canvas.className = "nb-mermaid-canvas";
        canvas.dataset.mermaidCanvas = "";
        figure.append(canvas);
      }
      canvas.innerHTML = svg;
      const renderedSvg = canvas.querySelector("svg");
      const viewBox = renderedSvg?.viewBox.baseVal;
      canvas.toggleAttribute(
        "data-mermaid-wide",
        Boolean(
          viewBox && viewBox.width > 900 && viewBox.width / viewBox.height > 1.8,
        ),
      );
      bindFunctions?.(canvas);
      figure.dataset.mermaidState = "rendered";
    } catch (error) {
      console.error("Unable to render Mermaid diagram", error);
    }
  }
}

function queueRender(): void {
  // Serialize renders because Mermaid owns global configuration and temporary
  // DOM state. The source block stays visible if a render fails.
  renderQueue = renderQueue.then(renderDiagrams, renderDiagrams);
}

export function initMermaidDiagrams(): void {
  if (!document.querySelector(diagramSelector)) return;

  const start = () => {
    void document.fonts.ready.then(queueRender);
  };
  if (document.readyState === "complete") start();
  else window.addEventListener("load", start, { once: true });

  let theme = currentTheme();
  new MutationObserver(() => {
    const nextTheme = currentTheme();
    if (nextTheme === theme) return;
    theme = nextTheme;
    queueRender();
  }).observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-mode"],
  });
}
