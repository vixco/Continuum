"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { Check, ChevronsUpDown, Loader2, RefreshCcw, Search } from "lucide-react";
import { clsx } from "clsx";

import { ProviderLogo } from "@/components/providers/ProviderLogo";
import { refreshAllProviderModels } from "@/lib/providers";
import type { ProviderConnection } from "@/lib/types";

interface ModelOption {
  key: string;
  model: string;
  provider: ProviderConnection;
}

export function ModelSwitcher({
  providers,
  providerId,
  model,
  onSelect,
}: {
  providers: ProviderConnection[];
  providerId: string;
  model: string;
  onSelect: (providerId: string, model: string) => Promise<void>;
}) {
  const rootRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [activeIndex, setActiveIndex] = useState(0);
  const [refreshing, setRefreshing] = useState(false);
  const [switching, setSwitching] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const activeProvider = providers.find((provider) => provider.id === providerId) ?? null;

  const options = useMemo<ModelOption[]>(
    () =>
      providers.flatMap((provider) => {
        const models = provider.models.length
          ? provider.models
          : provider.default_model
            ? [provider.default_model]
            : [];
        return [...new Set(models)].map((providerModel) => ({
          key: `${provider.id}:${providerModel}`,
          model: providerModel,
          provider,
        }));
      }),
    [providers]
  );
  const filtered = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    if (!needle) return options;
    return options.filter(
      (option) =>
        option.model.toLocaleLowerCase().includes(needle) ||
        option.provider.display_name.toLocaleLowerCase().includes(needle)
    );
  }, [options, query]);

  useEffect(() => {
    if (!open) return;
    searchRef.current?.focus();
    const onPointerDown = (event: MouseEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, [open]);

  useEffect(() => setActiveIndex(0), [query, open]);

  const choose = async (option: ModelOption) => {
    setSwitching(true);
    setError(null);
    try {
      await onSelect(option.provider.id, option.model);
      setOpen(false);
      setQuery("");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSwitching(false);
    }
  };

  return (
    <div ref={rootRef} className="relative">
      <button
        type="button"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => {
          setError(null);
          setOpen((value) => !value);
        }}
        className="press flex min-h-10 w-64 items-center gap-2 rounded-lg border border-bg-border bg-bg-elevated px-2.5 text-left transition-colors hover:border-bg-hover focus:outline-none focus-visible:ring-2 focus-visible:ring-accent-amber/40"
      >
        {activeProvider && <ProviderLogo provider={activeProvider} size={24} />}
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[12px] font-medium text-ink">
            {model || "Choose model"}
          </span>
          <span className="block truncate text-[10px] text-ink-dim">
            {activeProvider?.display_name ?? "All providers"}
          </span>
        </span>
        <ChevronsUpDown size={14} className="shrink-0 text-ink-dim" />
      </button>

      {open && (
        <div className="absolute right-0 top-full z-40 mt-2 w-80 overflow-hidden rounded-xl border border-bg-border bg-bg-surface shadow-2xl shadow-black/50">
          <div className="flex items-center gap-2 border-b border-bg-border p-2">
            <Search size={14} className="ml-1 shrink-0 text-ink-dim" />
            <input
              ref={searchRef}
              role="combobox"
              aria-label="Search models"
              aria-controls="chat-model-list"
              aria-expanded="true"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") setOpen(false);
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  setActiveIndex((index) => Math.min(index + 1, Math.max(0, filtered.length - 1)));
                }
                if (event.key === "ArrowUp") {
                  event.preventDefault();
                  setActiveIndex((index) => Math.max(index - 1, 0));
                }
                if (event.key === "Enter" && filtered[activeIndex]) {
                  event.preventDefault();
                  void choose(filtered[activeIndex]);
                }
              }}
              placeholder="Search model or provider…"
              className="h-9 min-w-0 flex-1 bg-transparent text-[12px] text-ink outline-none placeholder:text-ink-dim"
            />
            <button
              type="button"
              aria-label="Refresh all provider models"
              disabled={refreshing || switching}
              onClick={() => {
                setRefreshing(true);
                setError(null);
                void refreshAllProviderModels()
                  .catch((cause) =>
                    setError(cause instanceof Error ? cause.message : String(cause))
                  )
                  .finally(() => setRefreshing(false));
              }}
              className="press inline-flex h-9 w-9 items-center justify-center rounded-md text-ink-muted hover:bg-bg-hover hover:text-ink disabled:opacity-50"
            >
              {refreshing ? (
                <Loader2 size={14} className="animate-spin" />
              ) : (
                <RefreshCcw size={14} />
              )}
            </button>
          </div>
          <div id="chat-model-list" role="listbox" className="max-h-80 overflow-y-auto p-1.5">
            {filtered.length === 0 ? (
              <div className="px-3 py-8 text-center text-[11px] text-ink-dim">
                No matching models. Refresh providers or try another search.
              </div>
            ) : (
              filtered.map((option, index) => {
                const selected = option.provider.id === providerId && option.model === model;
                return (
                  <button
                    type="button"
                    role="option"
                    aria-selected={selected}
                    key={option.key}
                    onMouseEnter={() => setActiveIndex(index)}
                    disabled={switching}
                    onClick={() => void choose(option)}
                    className={clsx(
                      "flex min-h-11 w-full items-center gap-2.5 rounded-lg px-2.5 text-left transition-colors",
                      index === activeIndex ? "bg-bg-elevated" : "hover:bg-bg-elevated/70"
                    )}
                  >
                    <ProviderLogo provider={option.provider} size={28} />
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-[12px] font-medium text-ink">
                        {option.model}
                      </span>
                      <span className="block truncate text-[10px] text-ink-dim">
                        {option.provider.display_name}
                      </span>
                    </span>
                    {selected && <Check size={14} className="shrink-0 text-amber-400" />}
                  </button>
                );
              })
            )}
          </div>
          {error && (
            <div
              role="alert"
              className="border-t border-state-error/20 px-3 py-2 text-[10px] text-state-error"
            >
              {error}
            </div>
          )}
          <div className="border-t border-bg-border px-3 py-2 text-[10px] text-ink-dim">
            ↑↓ navigate · Enter select · Esc close
          </div>
        </div>
      )}
    </div>
  );
}
