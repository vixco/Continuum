"use client";

import { clsx } from "clsx";
import type {
  ButtonHTMLAttributes,
  InputHTMLAttributes,
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
        "rounded-lg border border-bg-border bg-bg-surface",
        dense ? "p-3" : "p-4",
        className,
      )}
    >
      {(title || actions) && (
        <div className="mb-3 flex items-start justify-between gap-2">
          <div>
            {title && (
              <div className="text-sm font-medium text-ink">{title}</div>
            )}
            {subtitle && (
              <div className="mt-0.5 text-xs text-ink-muted">{subtitle}</div>
            )}
          </div>
          {actions && <div className="flex items-center gap-1">{actions}</div>}
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

export function StatusBadge({
  status,
  label,
}: {
  status: ComponentStatus;
  label?: string;
}) {
  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-[11px] font-medium",
        STATUS_STYLES[status],
      )}
    >
      <span
        className={clsx(
          "h-1.5 w-1.5 rounded-full",
          status === "healthy" && "bg-state-healthy",
          status === "degrading" && "bg-state-warn",
          status === "error" && "bg-state-error",
          status === "unknown" && "bg-state-idle",
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
        "inline-flex items-center justify-center gap-2 rounded-md border font-medium transition-colors",
        "disabled:cursor-not-allowed disabled:opacity-50",
        size === "sm" && "px-2.5 py-1 text-xs",
        size === "md" && "px-3 py-1.5 text-sm",
        size === "lg" && "px-4 py-2 text-sm",
        variant === "default" &&
          "border-bg-border bg-bg-elevated text-ink hover:bg-bg-hover",
        variant === "primary" &&
          "border-accent-purple bg-accent-purple text-white hover:bg-accent-purple-dim",
        variant === "danger" &&
          "border-state-error bg-state-error/20 text-state-error hover:bg-state-error/30",
        variant === "ghost" &&
          "border-transparent bg-transparent text-ink-muted hover:bg-bg-hover hover:text-ink",
        className,
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
    <label className="inline-flex items-center gap-2.5 cursor-pointer">
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        disabled={disabled}
        onClick={() => onChange(!checked)}
        className={clsx(
          "relative h-5 w-9 rounded-full border transition-colors",
          "disabled:cursor-not-allowed disabled:opacity-50",
          checked
            ? "bg-accent-purple border-accent-purple"
            : "bg-bg-elevated border-bg-border",
        )}
      >
        <span
          className={clsx(
            "absolute top-0.5 h-3.5 w-3.5 rounded-full bg-white transition-transform",
            checked ? "translate-x-[18px]" : "translate-x-0.5",
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
}: {
  value: number;
  onChange: (v: number) => void;
  min?: number;
  max?: number;
  step?: number;
  label?: ReactNode;
  format?: (v: number) => string;
  disabled?: boolean;
}) {
  return (
    <div className="w-full">
      {label && (
        <div className="mb-1 flex items-center justify-between text-xs text-ink-muted">
          <span>{label}</span>
          <span className="font-mono text-ink">
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
        onChange={(e) => onChange(Number(e.target.value))}
        className="w-full h-1 appearance-none rounded-full bg-bg-elevated accent-accent-purple disabled:opacity-50"
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
  ...props
}: Omit<SelectHTMLAttributes<HTMLSelectElement>, "onChange" | "value"> & {
  value: T;
  options: Array<{ value: T; label: string }>;
  onChange: (v: T) => void;
  label?: ReactNode;
}) {
  return (
    <label className="block">
      {label && (
        <span className="mb-1 block text-xs text-ink-muted">{label}</span>
      )}
      <select
        {...props}
        value={value}
        onChange={(e) => onChange(e.target.value as T)}
        className="w-full rounded-md border border-bg-border bg-bg-elevated px-2.5 py-1.5 text-sm text-ink focus:border-accent-purple focus:outline-none"
      >
        {options.map((opt) => (
          <option key={opt.value} value={opt.value}>
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
  placeholder,
  className,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  className?: string;
}) {
  return (
    <input
      type="search"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      className={clsx(
        "w-full rounded-md border border-bg-border bg-bg-elevated px-2.5 py-1.5 text-sm",
        "text-ink placeholder:text-ink-dim focus:border-accent-purple focus:outline-none",
        className,
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
        "w-full rounded-md border border-bg-border bg-bg-elevated px-2.5 py-1.5 text-sm",
        "text-ink placeholder:text-ink-dim focus:border-accent-purple focus:outline-none",
        props.className,
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
  if (!open) return null;
  const widthClass =
    width === "sm" ? "max-w-sm" : width === "lg" ? "max-w-2xl" : "max-w-md";
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      onClick={onClose}
    >
      <div
        className={clsx(
          "w-full rounded-lg border border-bg-border bg-bg-surface shadow-xl",
          widthClass,
        )}
        onClick={(e) => e.stopPropagation()}
      >
        {title && (
          <div className="border-b border-bg-border px-4 py-3 text-sm font-medium">
            {title}
          </div>
        )}
        <div className="p-4">{children}</div>
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
  thinking: "bg-accent-purple",
  speaking: "bg-state-healthy",
  muted: "bg-ink-subtle",
  error: "bg-state-error",
};

export function StatusOrb({
  mode,
  size = "md",
}: {
  mode: VoiceMode;
  size?: "sm" | "md" | "lg";
}) {
  const sizeClass =
    size === "sm" ? "h-3 w-3" : size === "lg" ? "h-16 w-16" : "h-8 w-8";
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
          "rounded-full blur-lg absolute inset-0 opacity-60",
          ORB_COLOR[mode],
        )}
      />
      <span
        className={clsx(
          "relative rounded-full shadow-lg",
          sizeClass,
          ORB_COLOR[mode],
          animClass,
        )}
      />
    </div>
  );
}

// --- Simple helper types ---

export function Kbd({ children }: PropsWithChildren) {
  return (
    <kbd className="rounded border border-bg-border bg-bg-elevated px-1.5 py-0.5 font-mono text-[10px] text-ink-muted">
      {children}
    </kbd>
  );
}

export function EmptyState({
  title,
  description,
}: {
  title: string;
  description?: string;
}) {
  return (
    <div className="flex h-32 flex-col items-center justify-center text-center">
      <div className="text-sm text-ink-muted">{title}</div>
      {description && (
        <div className="mt-1 text-xs text-ink-dim">{description}</div>
      )}
    </div>
  );
}
