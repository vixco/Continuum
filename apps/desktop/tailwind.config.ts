import type { Config } from "tailwindcss";

const config: Config = {
  content: [
    "./src/**/*.{js,ts,jsx,tsx,mdx}",
  ],
  theme: {
    extend: {
      colors: {
        kairo: {
          bg: "#0a0a0f",
          surface: "#14141f",
          border: "#1e1e2e",
          text: "#e4e4ef",
          muted: "#6b6b8a",
          accent: "#7c6ff0",
        },
      },
    },
  },
  plugins: [],
};

export default config;
