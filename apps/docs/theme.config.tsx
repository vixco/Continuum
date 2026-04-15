import type { DocsThemeConfig } from "nextra-theme-docs";

const config: DocsThemeConfig = {
  logo: (
    <span style={{ fontWeight: 600, letterSpacing: "-0.02em" }}>
      K<span style={{ color: "#a855f7" }}>AI</span>ro
      <span style={{ marginLeft: 8, color: "#71717a", fontWeight: 400, fontSize: "0.85em" }}>
        docs
      </span>
    </span>
  ),
  project: {
    link: "https://github.com/vixco/kairo-ai",
  },
  docsRepositoryBase:
    "https://github.com/vixco/kairo-ai/tree/main/apps/docs",
  footer: {
    content: (
      <span>
        Apache 2.0 licensed. Built in Breda by{" "}
        <a href="https://github.com/vixco" target="_blank" rel="noopener noreferrer">
          Toshan
        </a>{" "}
        with help from Claude.
      </span>
    ),
  },
  head: (
    <>
      <meta name="viewport" content="width=device-width, initial-scale=1.0" />
      <meta
        name="description"
        content="Kairo — the AI that knows when to act. A desktop-native, local-first ambient AI assistant for Windows."
      />
      <meta property="og:title" content="Kairo" />
      <meta
        property="og:description"
        content="The AI that knows when to act. A desktop-native, local-first ambient AI assistant for Windows."
      />
    </>
  ),
  sidebar: {
    defaultMenuCollapseLevel: 1,
    toggleButton: true,
  },
  darkMode: true,
  nextThemes: {
    defaultTheme: "dark",
  },
  primaryHue: 280,
  primarySaturation: 60,
  useNextSeoProps() {
    return {
      titleTemplate: "%s — Kairo docs",
    };
  },
};

export default config;
