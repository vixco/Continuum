"use client";

import type { ReactNode } from "react";
import { clsx } from "clsx";
import { ChevronRight } from "lucide-react";

export function Panel({
  children,
  className,
  title,
  action,
}: {
  children: ReactNode;
  className?: string;
  title?: string;
  action?: ReactNode;
}) {
  return (
    <section className={clsx("continuum-panel", className)}>
      {(title || action) && (
        <div className="mb-4 flex items-center justify-between gap-3">
          {title && <h2 className="text-[13px] font-semibold text-white/90">{title}</h2>}
          {action}
        </div>
      )}
      {children}
    </section>
  );
}

export function TextAction({ children }: { children: ReactNode }) {
  return (
    <button className="inline-flex min-h-8 items-center gap-1.5 rounded-md px-2 text-[11px] font-medium text-amber-400 transition-colors hover:bg-amber-400/10 hover:text-amber-300 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-amber-400/70">
      {children}
      <ChevronRight size={13} />
    </button>
  );
}

export function Button({
  children,
  variant = "ghost",
  className,
  onClick,
}: {
  children: ReactNode;
  variant?: "primary" | "ghost" | "danger";
  className?: string;
  onClick?: () => void;
}) {
  return (
    <button
      onClick={onClick}
      className={clsx(
        "inline-flex min-h-10 cursor-pointer items-center justify-center gap-2 rounded-lg border px-4 text-[12px] font-medium transition-all duration-200 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-amber-400/70",
        variant === "primary" &&
          "border-amber-400/60 bg-gradient-to-b from-amber-400 to-amber-600 text-black shadow-[0_0_24px_rgba(245,158,11,.18)] hover:brightness-110",
        variant === "ghost" &&
          "border-white/[.09] bg-white/[.025] text-white/85 hover:border-amber-500/30 hover:bg-amber-500/[.06]",
        variant === "danger" &&
          "border-red-500/25 bg-red-500/[.04] text-red-400 hover:bg-red-500/10",
        className
      )}
    >
      {children}
    </button>
  );
}

export function Badge({
  children,
  tone = "neutral",
}: {
  children: ReactNode;
  tone?: "green" | "amber" | "red" | "blue" | "violet" | "neutral";
}) {
  return (
    <span
      className={clsx(
        "inline-flex items-center rounded-md border px-2 py-1 text-[10px] font-medium",
        tone === "green" && "border-emerald-500/10 bg-emerald-500/10 text-emerald-400",
        tone === "amber" && "border-amber-500/10 bg-amber-500/10 text-amber-400",
        tone === "red" && "border-red-500/10 bg-red-500/10 text-red-400",
        tone === "blue" && "border-sky-500/10 bg-sky-500/10 text-sky-400",
        tone === "violet" && "border-violet-500/10 bg-violet-500/10 text-violet-400",
        tone === "neutral" && "border-white/[.06] bg-white/[.04] text-white/55"
      )}
    >
      {children}
    </span>
  );
}

export function Progress({ value, className }: { value: number; className?: string }) {
  return (
    <div className={clsx("h-1.5 overflow-hidden rounded-full bg-white/[.07]", className)}>
      <div
        className="h-full rounded-full bg-gradient-to-r from-amber-600 via-amber-400 to-yellow-300 shadow-[0_0_10px_rgba(245,158,11,.45)]"
        style={{ width: `${Math.max(0, Math.min(100, value))}%` }}
      />
    </div>
  );
}

export function Ring({ value, label }: { value: number; label?: string }) {
  return (
    <div
      className="relative grid h-[92px] w-[92px] place-items-center rounded-full p-[5px] shadow-[0_0_24px_rgba(245,158,11,.12)]"
      style={{
        background: `conic-gradient(#fbbf24 ${value * 3.6}deg, #d97706 ${value * 3.6}deg, rgba(255,255,255,.07) 0deg)`,
      }}
    >
      <div className="grid h-full w-full place-items-center rounded-full bg-[#11120f] text-center">
        <div>
          <div className="text-[23px] font-medium tracking-tight text-white">{value}%</div>
          {label && <div className="text-[10px] text-emerald-400">{label}</div>}
        </div>
      </div>
    </div>
  );
}

export function StatCard({
  icon,
  label,
  value,
  meta,
  tone = "amber",
}: {
  icon: ReactNode;
  label: string;
  value: string;
  meta: string;
  tone?: "amber" | "green" | "red";
}) {
  return (
    <Panel className="flex min-h-[88px] items-center gap-4 p-4">
      <div
        className={clsx(
          "grid h-10 w-10 shrink-0 place-items-center rounded-full",
          tone === "amber" && "bg-amber-500/10 text-amber-400",
          tone === "green" && "bg-emerald-500/10 text-emerald-400",
          tone === "red" && "bg-red-500/10 text-red-400"
        )}
      >
        {icon}
      </div>
      <div>
        <div className="text-[11px] text-white/55">{label}</div>
        <div className="text-[23px] font-medium leading-tight text-white">{value}</div>
        <div className="mt-1 text-[10px] text-white/40">{meta}</div>
      </div>
    </Panel>
  );
}

export function Dot({ tone = "green" }: { tone?: "green" | "amber" | "red" | "gray" }) {
  return (
    <span
      className={clsx(
        "inline-block h-1.5 w-1.5 rounded-full",
        tone === "green" && "bg-emerald-400 shadow-[0_0_7px_rgba(52,211,153,.65)]",
        tone === "amber" && "bg-amber-400 shadow-[0_0_7px_rgba(251,191,36,.65)]",
        tone === "red" && "bg-red-400 shadow-[0_0_7px_rgba(248,113,113,.65)]",
        tone === "gray" && "bg-white/35"
      )}
    />
  );
}

export function EmptyPage({
  title,
  description,
  children,
}: {
  title: string;
  description: string;
  children?: ReactNode;
}) {
  return (
    <div className="mx-auto flex min-h-[70vh] max-w-4xl items-center justify-center">
      <Panel className="w-full p-10 text-center">
        <div className="mx-auto mb-5 h-16 w-16 rounded-2xl border border-amber-400/20 bg-amber-400/[.06] shadow-[0_0_40px_rgba(245,158,11,.08)]" />
        <h1 className="text-2xl font-semibold text-white">{title}</h1>
        <p className="mx-auto mt-3 max-w-xl text-sm leading-6 text-white/50">{description}</p>
        {children}
      </Panel>
    </div>
  );
}
