import type { Config } from "tailwindcss";

const config: Config = {
  content: ["./src/**/*.{js,ts,jsx,tsx,mdx}"],
  theme: {
    extend: {
      colors: {
        bg: {
          DEFAULT: "#0a0a0f",
          surface: "#121218",
          elevated: "#1a1a24",
          border: "#22222e",
          hover: "#232330",
        },
        accent: {
          purple: "#7c3aed",
          "purple-dim": "#5b21b6",
          blue: "#3b82f6",
          "blue-dim": "#1d4ed8",
        },
        state: {
          healthy: "#22c55e",
          warn: "#f59e0b",
          error: "#ef4444",
          idle: "#6b7280",
        },
        ink: {
          DEFAULT: "#e5e7eb",
          muted: "#9ca3af",
          dim: "#6b7280",
          subtle: "#4b5563",
        },
      },
      fontFamily: {
        sans: [
          "Inter",
          "-apple-system",
          "BlinkMacSystemFont",
          "Segoe UI",
          "sans-serif",
        ],
        mono: ["JetBrains Mono", "Consolas", "Monaco", "monospace"],
      },
      animation: {
        "pulse-slow": "pulse 3s ease-in-out infinite",
        "orb-thinking": "orb-thinking 2s ease-in-out infinite",
        "orb-speaking": "orb-speaking 0.6s ease-in-out infinite",
      },
      keyframes: {
        "orb-thinking": {
          "0%, 100%": { transform: "scale(1)", opacity: "0.85" },
          "50%": { transform: "scale(1.1)", opacity: "1" },
        },
        "orb-speaking": {
          "0%, 100%": { transform: "scale(1)" },
          "50%": { transform: "scale(1.08)" },
        },
      },
    },
  },
  plugins: [],
};

export default config;
