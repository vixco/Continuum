"use client";

import { clsx } from "clsx";
import { useRef } from "react";
import type {
  ButtonHTMLAttributes,
  InputHTMLAttributes,
  KeyboardEvent,
  PropsWithChildren,
  ReactNode,
  SelectHTMLAttributes,
} from "react";

import type { ComponentStatus, VoiceMode } from "@/lib/types";

// --- Card ---

export function Card({
  title,
  subtitle,
  children,
  actions,
  className,
  dense,
}: PropsWithChildren<{
  title?: ReactNode;
  subtitle?: ReactNode;
  actions?: ReactNode;
  className?: string;
  dense?: boolean;
}>) {
  return (
    <div
      className={clsx(
        "continuum-card rounded-[var(--radius-card)] border border-bg-border bg-bg-surface",
        dense ? "p-4" : "p-6",
        className
      )}
    >
      {(title || actions) && (
        <div className="mb-5 flex items-start justify-between gap-4">
          <div className="min-w-0">
            {title && (
              <div className="font-display text-sm font-bold tracking-[-0.01em] text-ink">
                {title}
              </div>
            )}
            {subtitle && (
              <div className="mt-1.5 text-[13px] leading-5 text-ink-dim">{subtitle}</div>
            )}
          </div>
          {actions && <div className="flex shrink-0 items-center gap-1.5">{actions}</div>}
        </div>
      )}
      {children}
    </div>
  );
}

// --- StatusBadge ---

const STATUS_STYLES: Record<ComponentStatus, string> = {
  healthy: "bg-state-healthy/15 text-state-healthy border-state-healthy/30",
  degrading: "bg-state-warn/15 text-state-warn border-state-warn/30",
  error: "bg-state-error/15 text-state-error border-state-error/30",
  unknown: "bg-state-idle/15 text-state-idle border-state-idle/30",
};

const STATUS_LABEL: Record<ComponentStatus, string> = {
  healthy: "Healthy",
  degrading: "Degrading",
  error: "Error",
  unknown: "Unknown",
};

export function StatusBadge({ status, label }: { status: ComponentStatus; label?: string }) {
  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-[11px] font-medium",
        STATUS_STYLES[status]
      )}
    >
      <span
        className={clsx(
          "h-1.5 w-1.5 rounded-full",
          status === "healthy" && "bg-state-healthy",
          status === "degrading" && "bg-state-warn",
          status === "error" && "bg-state-error",
          status === "unknown" && "bg-state-idle"
        )}
      />
      {label ?? STATUS_LABEL[status]}
    </span>
  );
}

// --- Button ---

export function Button({
  variant = "default",
  size = "md",
  className,
  children,
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "default" | "primary" | "danger" | "ghost";
  size?: "sm" | "md" | "lg";
}) {
  return (
    <button
      {...props}
      className={clsx(
        "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-[var(--radius-input)] border font-semibold",
        "transition-[transform,background-color,border-color,color] duration-200 ease-[var(--ease-out)] active:translate-y-px",
        "focus:outline-none focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
        "disabled:cursor-not-allowed disabled:opacity-50 disabled:active:translate-y-0",
        size === "sm" && "min-h-9 px-3 text-xs",
        size === "md" && "min-h-10 px-3.5 text-sm",
        size === "lg" && "min-h-11 px-4 text-sm",
        variant === "default" &&
          "border-bg-border bg-bg-elevated text-ink hover:border-bg-hover hover:bg-bg-hover",
        variant === "primary" &&
          "border-accent-amber bg-accent-amber text-accent-ink hover:bg-accent-amber-dim",
        variant === "danger" &&
          "border-state-error/40 bg-state-error/15 text-state-error hover:bg-state-error/25",
        variant === "ghost" &&
          "border-transparent bg-transparent text-ink-muted hover:bg-bg-hover hover:text-ink",
        className
      )}
    >
      {children}
    </button>
  );
}

// --- Toggle ---

export function Toggle({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  label?: ReactNode;
  disabled?: boolean;
}) {
  return (
    <label className="inline-flex cursor-pointer select-none items-center gap-2.5">
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        disabled={disabled}
        onClick={() => onChange(!checked)}
        className={clsx(
          "relative inline-flex h-7 w-12 shrink-0 items-center rounded-full border px-1 transition-colors duration-200 ease-[var(--ease-out)] after:absolute after:-inset-y-2 after:inset-x-0 active:translate-y-px",
          "focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
          "disabled:cursor-not-allowed disabled:opacity-50",
          checked
            ? "border-accent-amber bg-accent-amber"
            : "border-bg-border bg-bg-elevated hover:border-bg-hover"
        )}
      >
        <span
          className={clsx(
            "block h-4 w-4 rounded-full bg-accent-ink transition-transform duration-150 ease-[var(--ease-out)]",
            checked ? "translate-x-5" : "translate-x-0"
          )}
        />
      </button>
      {label && <span className="text-sm text-ink">{label}</span>}
    </label>
  );
}

// --- Slider ---

export function Slider({
  value,
  onChange,
  min = 0,
  max = 1,
  step = 0.01,
  label,
  format,
  disabled,
  title,
}: {
  value: number;
  onChange: (v: number) => void;
  min?: number;
  max?: number;
  step?: number;
  label?: ReactNode;
  format?: (v: number) => string;
  disabled?: boolean;
  /** HTML title attribute (native tooltip) — used to surface "not yet wired"
   *  hints on sliders whose underlying control lives elsewhere. */
  title?: string;
}) {
  const fillPct = max === min ? 0 : ((value - min) / (max - min)) * 100;
  return (
    <div className="w-full">
      {label && (
        <div className="mb-1.5 flex items-center justify-between text-xs text-ink-muted">
          <span>{label}</span>
          <span className="font-mono tabular-nums text-ink">
            {format ? format(value) : value.toFixed(2)}
          </span>
        </div>
      )}
      <input
        type="range"
        value={value}
        min={min}
        max={max}
        step={step}
        disabled={disabled}
        title={title}
        onChange={(e) => onChange(Number(e.target.value))}
        style={{ ["--continuum-range-fill" as string]: `${fillPct}%` }}
        className="continuum-range block w-full"
      />
    </div>
  );
}

// --- Select ---

export function Select<T extends string>({
  value,
  options,
  onChange,
  label,
  className,
  ...props
}: Omit<SelectHTMLAttributes<HTMLSelectElement>, "onChange" | "value"> & {
  value: T;
  options: Array<{ value: T; label: string; disabled?: boolean }>;
  onChange: (v: T) => void;
  label?: ReactNode;
}) {
  // The caller's className sizes the outer label (the select itself stays
  // w-full inside it), so width utilities like `w-28` don't fight `w-full`.
  return (
    <label className={clsx("block", className)}>
      {label && <span className="mb-1.5 block text-xs text-ink-muted">{label}</span>}
      <select
        {...props}
        value={value}
        onChange={(e) => onChange(e.target.value as T)}
        className={clsx(
          "continuum-select min-h-11 w-full rounded-[var(--radius-input)] border border-bg-border bg-bg-elevated py-2 pl-3 pr-8 text-sm text-ink",
          "transition-colors duration-200 ease-[var(--ease-out)] hover:bg-bg-hover",
          "focus:outline focus:outline-2 focus:outline-offset-1 focus:outline-[var(--color-focus)]"
        )}
      >
        {options.map((opt) => (
          <option key={opt.value} value={opt.value} disabled={opt.disabled}>
            {opt.label}
          </option>
        ))}
      </select>
    </label>
  );
}

// --- Input ---

export function SearchInput({
  value,
  onChange,
  onKeyDown,
  placeholder,
  className,
}: {
  value: string;
  onChange: (v: string) => void;
  onKeyDown?: (e: KeyboardEvent<HTMLInputElement>) => void;
  placeholder?: string;
  className?: string;
}) {
  return (
    <input
      type="search"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      onKeyDown={onKeyDown}
      placeholder={placeholder}
      className={clsx(
        "min-h-11 w-full rounded-[var(--radius-input)] border border-bg-border bg-bg-elevated px-3 text-sm",
        "text-ink transition-colors duration-200 ease-[var(--ease-out)] placeholder:text-ink-dim",
        "hover:bg-bg-hover",
        "focus:outline focus:outline-2 focus:outline-offset-1 focus:outline-[var(--color-focus)]",
        className
      )}
    />
  );
}

export function TextInput({
  value,
  onChange,
  placeholder,
  ...props
}: Omit<InputHTMLAttributes<HTMLInputElement>, "onChange" | "value"> & {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
}) {
  return (
    <input
      type="text"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      {...props}
      className={clsx(
        "min-h-11 w-full rounded-[var(--radius-input)] border border-bg-border bg-bg-elevated px-3 text-sm",
        "text-ink transition-colors duration-200 ease-[var(--ease-out)] placeholder:text-ink-dim",
        "hover:bg-bg-hover",
        "focus:outline focus:outline-2 focus:outline-offset-1 focus:outline-[var(--color-focus)]",
        props.className
      )}
    />
  );
}

// --- Modal ---

export function Modal({
  open,
  onClose,
  title,
  children,
  footer,
  width = "md",
}: PropsWithChildren<{
  open: boolean;
  onClose: () => void;
  title?: ReactNode;
  footer?: ReactNode;
  width?: "sm" | "md" | "lg";
}>) {
  // Close only when the press both started AND ended on the backdrop.
  // A text-selection drag that starts inside the panel and releases outside
  // dispatches its click on the backdrop, which must not dismiss the modal.
  const pressStartedOnBackdrop = useRef(false);
  if (!open) return null;
  const widthClass = width === "sm" ? "max-w-sm" : width === "lg" ? "max-w-2xl" : "max-w-md";
  return (
    <div
      className="fixed inset-0 z-[var(--z-modal)] flex items-center justify-center bg-bg/85 p-4"
      onMouseDown={(e) => {
        pressStartedOnBackdrop.current = e.target === e.currentTarget;
      }}
      onClick={(e) => {
        if (e.target === e.currentTarget && pressStartedOnBackdrop.current) onClose();
        pressStartedOnBackdrop.current = false;
      }}
    >
      <div
        className={clsx(
          "continuum-modal flex max-h-[min(82dvh,44rem)] w-full flex-col overflow-hidden rounded-[var(--radius-card)] border border-bg-border bg-bg-elevated",
          widthClass
        )}
        onClick={(e) => e.stopPropagation()}
      >
        {title && (
          <div className="border-b border-bg-border px-4 py-3 text-sm font-medium">{title}</div>
        )}
        <div className="min-h-0 overflow-y-auto p-4">{children}</div>
        {footer && (
          <div className="flex items-center justify-end gap-2 border-t border-bg-border px-4 py-3">
            {footer}
          </div>
        )}
      </div>
    </div>
  );
}

// --- Status orb (used on Home tab + topbar) ---

const ORB_COLOR: Record<VoiceMode, string> = {
  idle: "bg-state-idle",
  listening: "bg-accent-blue",
  thinking: "bg-accent-amber",
  speaking: "bg-state-healthy",
  muted: "bg-ink-subtle",
  error: "bg-state-error",
};

export function StatusOrb({ mode, size = "md" }: { mode: VoiceMode; size?: "sm" | "md" | "lg" }) {
  const sizeClass = size === "sm" ? "h-3 w-3" : size === "lg" ? "h-16 w-16" : "h-8 w-8";
  const animClass =
    mode === "listening"
      ? "animate-pulse-slow"
      : mode === "thinking"
        ? "animate-orb-thinking"
        : mode === "speaking"
          ? "animate-orb-speaking"
          : "";
  return (
    <div className="relative inline-flex items-center justify-center">
      <span
        className={clsx(
          "relative rounded-full ring-1 ring-inset ring-ink/15",
          sizeClass,
          ORB_COLOR[mode],
          animClass
        )}
      />
    </div>
  );
}

// --- Simple helper types ---

export function Kbd({ children, className }: PropsWithChildren<{ className?: string }>) {
  return (
    <kbd
      className={clsx(
        "rounded border border-bg-border bg-bg-elevated px-1.5 py-0.5 font-mono text-[11px] tabular-nums text-ink-muted",
        className
      )}
    >
      {children}
    </kbd>
  );
}

export function EmptyState({ title, description }: { title: string; description?: string }) {
  return (
    <div className="flex min-h-36 flex-col items-center justify-center px-6 text-center">
      <div className="font-display text-sm font-bold text-ink">{title}</div>
      {description && (
        <div className="mt-2 max-w-md text-sm leading-6 text-ink-dim">{description}</div>
      )}
    </div>
  );
}
