import nextra from "nextra";

const withNextra = nextra({
  theme: "nextra-theme-docs",
  themeConfig: "./theme.config.tsx",
  defaultShowCopyCode: true,
});

const basePath = process.env.NEXT_PUBLIC_BASE_PATH || "";

export default withNextra({
  output: "export",
  images: { unoptimized: true },
  basePath,
  assetPrefix: basePath,
  trailingSlash: true,
  typescript: {
    // Nextra's _meta.{ts,tsx} files don't satisfy Next.js's PagesPageConfig
    // type, so the generated validator.ts rejects them. They aren't real
    // Next.js pages — Nextra intercepts them — so skipping build-time type
    // checks here is safe. tsc still catches errors via `pnpm typecheck`.
    ignoreBuildErrors: true,
  },
});
