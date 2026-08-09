import type { Config } from "tailwindcss";

// Continuum accent runtime mapping. Full values live in /tokens.css;
// channel tokens retain Tailwind 3 opacity modifiers such as bg-bg-surface/70.

const config: Config = {
  content: ["./src/**/*.{js,ts,jsx,tsx,mdx}"],
  theme: {
    extend: {
      colors: {
        amber: {
          300: "oklch(var(--accent-channels) / <alpha-value>)",
          400: "oklch(var(--accent-channels) / <alpha-value>)",
          500: "oklch(var(--accent-channels) / <alpha-value>)",
        },
        red: {
          200: "oklch(var(--error-channels) / <alpha-value>)",
          300: "oklch(var(--error-channels) / <alpha-value>)",
          400: "oklch(var(--error-channels) / <alpha-value>)",
        },
        bg: {
          DEFAULT: "oklch(var(--paper-channels) / <alpha-value>)",
          surface: "oklch(var(--paper-2-channels) / <alpha-value>)",
          elevated: "oklch(var(--paper-3-channels) / <alpha-value>)",
          border: "oklch(var(--rule-channels) / <alpha-value>)",
          hover: "oklch(var(--hover-channels) / <alpha-value>)",
        },
        accent: {
          amber: "oklch(var(--accent-channels) / <alpha-value>)",
          "amber-dim": "oklch(var(--accent-dim-channels) / <alpha-value>)",
          ink: "var(--color-accent-ink)",
          blue: "oklch(var(--voice-channels) / <alpha-value>)",
          "blue-dim": "oklch(var(--voice-dim-channels) / <alpha-value>)",
        },
        state: {
          healthy: "oklch(var(--healthy-channels) / <alpha-value>)",
          warn: "oklch(var(--warning-channels) / <alpha-value>)",
          error: "oklch(var(--error-channels) / <alpha-value>)",
          idle: "oklch(var(--idle-channels) / <alpha-value>)",
        },
        ink: {
          DEFAULT: "oklch(var(--ink-channels) / <alpha-value>)",
          muted: "oklch(var(--ink-2-channels) / <alpha-value>)",
          dim: "oklch(var(--muted-channels) / <alpha-value>)",
          subtle: "oklch(var(--neutral-channels) / <alpha-value>)",
        },
      },
      fontFamily: {
        sans: ["var(--font-body)"],
        display: ["var(--font-display)"],
        mono: ["var(--font-outlier)"],
      },
      animation: {
        "pulse-slow": "pulse 3s var(--ease-in-out) infinite",
        "orb-thinking": "orb-thinking 2s var(--ease-in-out) infinite",
        "orb-speaking": "orb-speaking 0.8s var(--ease-in-out) infinite",
      },
      keyframes: {
        "orb-thinking": {
          "0%, 100%": { opacity: "0.58" },
          "50%": { opacity: "1" },
        },
        "orb-speaking": {
          "0%, 100%": { opacity: "0.72" },
          "50%": { opacity: "1" },
        },
      },
    },
  },
  plugins: [],
};

export default config;
