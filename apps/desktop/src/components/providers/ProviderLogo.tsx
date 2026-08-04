"use client";

/* eslint-disable @next/next/no-img-element -- Bundled provider favicons are tiny local raster brand assets; Next image optimization is unavailable in the static Tauri export. */

import { Bot } from "lucide-react";

import type { ProviderConnection } from "@/lib/types";

const LOGO_BY_CATALOG: Record<string, string> = {
  lmstudio: "lmstudio.png",
  ollama: "ollama.png",
  "claude-cli": "anthropic.png",
  anthropic: "anthropic.png",
  openai: "openai.png",
  openrouter: "openrouter.png",
  deepseek: "deepseek.png",
  fireworks: "fireworks.png",
  kimi: "moonshot.png",
  "kimi-cn": "moonshot.png",
  zai: "zai.png",
  minimax: "minimax.png",
  xai: "xai.png",
  stepfun: "stepfun.png",
  nvidia: "nvidia.jpg",
  huggingface: "huggingface.png",
  gemini: "google.png",
  dashscope: "qwen.png",
};

export function ProviderLogo({
  provider,
  size = 24,
}: {
  provider: Pick<ProviderConnection, "catalog_id" | "display_name">;
  size?: number;
}) {
  const logo = provider.catalog_id ? LOGO_BY_CATALOG[provider.catalog_id] : undefined;
  if (!logo) {
    return (
      <span
        className="inline-flex shrink-0 items-center justify-center rounded-md border border-bg-border bg-bg-elevated text-ink-dim"
        style={{ width: size, height: size }}
        aria-hidden="true"
      >
        <Bot size={Math.max(12, size - 10)} />
      </span>
    );
  }
  return (
    <img
      src={`/provider-logos/${logo}`}
      alt=""
      width={size}
      height={size}
      draggable={false}
      className="shrink-0 rounded-md bg-white object-contain p-0.5"
    />
  );
}
