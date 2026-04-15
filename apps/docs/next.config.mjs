import nextra from "nextra";

const withNextra = nextra({
  defaultShowCopyCode: true,
});

const basePath = process.env.NEXT_PUBLIC_BASE_PATH || "";

export default withNextra({
  output: "export",
  images: { unoptimized: true },
  basePath,
  assetPrefix: basePath,
  trailingSlash: true,
});
