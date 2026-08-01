import type { Config } from "tailwindcss";

const config: Config = {
  content: ["./src/**/*.{js,ts,jsx,tsx,mdx}"],
  theme: {
    extend: {
      colors: {
        bg: {
          DEFAULT: "#090a09",
          surface: "#111210",
          elevated: "#171814",
          border: "rgba(255,255,255,.075)",
          hover: "#20211d",
        },
        accent: {
          purple: "#f59e0b",
          "purple-dim": "#b45309",
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
          DEFAULT: "#f4f4f2",
          muted: "#a1a19a",
          dim: "#6f7069",
          subtle: "#4b4c47",
        },
      },
      fontFamily: {
        sans: ["Inter", "-apple-system", "BlinkMacSystemFont", "Segoe UI", "sans-serif"],
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
