// OnboardingWizard - first-run setup for Continuum.
// Mounted by Shell when `is_onboarding_complete` returns false. Minimal,
// soft, animated single column. The top strip is a drag region so the
// frameless window can still be moved while onboarding covers the screen.
//
// Backend contract (apps/desktop/src-tauri/src/commands.rs):
//   check_claude_cli()         -> { installed, version, error }
//   check_claude_auth()        -> { authenticated, error }
//   list_audio_input_devices() -> [{ name, id }]
//   list_audio_output_devices()-> [{ name, id }]
//   download_model(name, url)  -> progress via `continuum:onboarding:progress`
//   run_diagnostics()          -> { checks: [{ name, status, detail }] }
//   is_onboarding_complete()   -> bool
//   complete_onboarding(payload)-> void

"use client";

import { useEffect, useMemo, useState } from "react";
import {
  Check,
  ChevronLeft,
  ChevronRight,
  AlertCircle,
  Loader2,
  Download,
  Mic,
  Volume2,
  ShieldCheck,
  User,
  Stethoscope,
  Sparkles,
  Boxes,
} from "lucide-react";
import { clsx } from "clsx";
import { invoke } from "@tauri-apps/api/core";

type StepId =
  | "welcome" | "claude" | "models" | "voice" | "permissions" | "personal" | "diagnostics" | "done";

const STEPS: Array<{ id: StepId; label: string; icon: typeof Sparkles }> = [
  { id: "welcome", label: "Welcome", icon: Sparkles },
  { id: "claude", label: "Claude Code", icon: Check },
  { id: "models", label: "Models", icon: Download },
  { id: "voice", label: "Voice", icon: Mic },
  { id: "permissions", label: "Permissions", icon: ShieldCheck },
  { id: "personal", label: "You", icon: User },
  { id: "diagnostics", label: "Diagnostics", icon: Stethoscope },
  { id: "done", label: "Done", icon: Sparkles },
];

export interface OnboardingPayload {
  name?: string;
  timezone?: string;
  language?: "en" | "nl" | "both";
  wake_word_enabled: boolean;
  wake_sensitivity: number;
  primary_voice: string;
  mic_device?: string;
  speaker_device?: string;
  permissions: "default" | "custom";
  extra_paths: string[];
}

const DEFAULT_PAYLOAD: OnboardingPayload = {
  wake_word_enabled: true,
  wake_sensitivity: 0.5,
  primary_voice: "en_US-norman-medium",
  permissions: "default",
  extra_paths: [],
  language: "en",
};

export function OnboardingWizard({ onComplete }: { onComplete: () => void }) {
  const [step, setStep] = useState<StepId>("welcome");
  const [payload, setPayload] = useState<OnboardingPayload>(() => ({
    ...DEFAULT_PAYLOAD,
    timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    language: detectLanguage(),
  }));

  const idx = STEPS.findIndex((s) => s.id === step);
  const goBack = () => setStep(STEPS[Math.max(0, idx - 1)].id);
  const goNext = () => setStep(STEPS[Math.min(STEPS.length - 1, idx + 1)].id);

  const finish = async () => {
    try {
      await invoke("complete_onboarding", { payload });
    } catch (err) {
      console.warn("complete_onboarding failed, proceeding anyway", err);
    }
    onComplete();
  };

  const isDone = step === "done";

  return (
    <div className="wizard">
      <div className="wizard-drag" data-tauri-drag-region>
        <span className="grid h-6 w-6 place-items-center text-amber-400">
          <Boxes size={16} />
        </span>
        <span className="text-[13px] font-semibold tracking-tight text-ink">Continuum</span>
        <span className="ml-2 text-[11px] text-ink-dim">first-run setup</span>
        <div className="ml-auto flex items-center gap-1.5">
          {STEPS.map((s, i) => (
            <span
              key={s.id}
              className={clsx("w-dot", i < idx && "done", i === idx && "active")}
              title={s.label}
            />
          ))}
        </div>
      </div>

      <div className="wizard-body">
        <div className="wizard-card" key={step}>
          <div className="wizard-step">
            {step === "welcome" && <WelcomeStep onNext={goNext} />}
            {step === "claude" && <ClaudeStep onNext={goNext} />}
            {step === "models" && <ModelsStep onNext={goNext} />}
            {step === "voice" && <VoiceStep payload={payload} setPayload={setPayload} />}
            {step === "permissions" && (
              <PermissionsStep payload={payload} setPayload={setPayload} />
            )}
            {step === "personal" && <PersonalStep payload={payload} setPayload={setPayload} />}
            {step === "diagnostics" && <DiagnosticsStep />}
            {step === "done" && <DoneStep onStart={finish} />}
          </div>
        </div>
      </div>

      <footer className="flex items-center justify-between px-6 pb-5 pt-1">
        <button
          disabled={idx <= 0 || isDone}
          onClick={goBack}
          className={clsx(
            "press inline-flex items-center gap-1.5 rounded-lg px-3 py-2 text-[13px]",
            idx > 0 && !isDone
              ? "text-ink-muted hover:bg-white/5 hover:text-ink"
              : "cursor-not-allowed opacity-30"
          )}
        >
          <ChevronLeft size={15} /> Back
        </button>
        <span className="text-[11px] text-ink-dim">
          {idx + 1} / {STEPS.length}
        </span>
        <button
          onClick={isDone ? finish : goNext}
          className="press inline-flex items-center gap-1.5 rounded-lg border border-amber-400/50 bg-amber-400/15 px-4 py-2 text-[13px] font-medium text-amber-200 hover:bg-amber-400/25"
        >
          {isDone ? "Start Continuum" : "Next"}
          <ChevronRight size={15} />
        </button>
      </footer>
    </div>
  );
}

function detectLanguage(): "en" | "nl" | "both" {
  if (typeof navigator === "undefined") return "en";
  const lang = navigator.language.toLowerCase();
  if (lang.startsWith("nl")) return "both";
  return "en";
}

// ---- Steps ------------------------------------------------------------------

function StepContainer({
  eyebrow,
  title,
  children,
}: {
  eyebrow?: string;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-5">
      <div>
        {eyebrow && (
          <p className="mb-2 text-[10px] font-semibold uppercase tracking-[0.14em] text-amber-400/80">
            {eyebrow}
          </p>
        )}
        <h1 className="text-[26px] font-semibold leading-tight tracking-tight text-ink">
          {title}
        </h1>
      </div>
      {children}
    </div>
  );
}

function WelcomeStep({ onNext: _onNext }: { onNext: () => void }) {
  return (
    <StepContainer eyebrow="Welcome" title="Meet Continuum">
      <p className="text-[14px] leading-relaxed text-ink-muted">
        An ambient AI assistant for Windows. It watches your screen, listens when spoken to,
        remembers what matters, and acts only when the moment is right. Local-first, powered by
        Claude Code.
      </p>
      <p className="text-[13px] text-ink-dim">
        Takes about ten minutes. Most of it is a one-time model download running in the background.
      </p>
      <ul className="flex flex-col gap-2 text-[13px] text-ink-muted">
        <Bullet>Needs a Claude Max or API subscription.</Bullet>
        <Bullet>Microphone and speaker recommended.</Bullet>
        <Bullet>About 6.5 GB of models download once.</Bullet>
      </ul>
    </StepContainer>
  );
}

function Bullet({ children }: { children: React.ReactNode }) {
  return (
    <li className="flex items-start gap-2.5">
      <Check size={14} className="mt-1 shrink-0 text-amber-400" />
      <span>{children}</span>
    </li>
  );
}

interface ClaudeCheckResult {
  installed: boolean;
  version: string | null;
  authenticated: boolean;
  error: string | null;
}

function ClaudeStep({ onNext: _onNext }: { onNext: () => void }) {
  const [state, setState] = useState<"idle" | "checking" | "ok" | "missing" | "unauth">("idle");
  const [result, setResult] = useState<ClaudeCheckResult | null>(null);

  const runCheck = async () => {
    setState("checking");
    try {
      const cli = await invoke<{
        installed: boolean;
        version: string | null;
        error: string | null;
      }>("check_claude_cli");
      const auth = await invoke<{ authenticated: boolean; error: string | null }>(
        "check_claude_auth"
      );
      const combined: ClaudeCheckResult = {
        installed: cli.installed,
        version: cli.version,
        authenticated: auth.authenticated,
        error: cli.error ?? auth.error,
      };
      setResult(combined);
      if (!combined.installed) setState("missing");
      else if (!combined.authenticated) setState("unauth");
      else setState("ok");
    } catch (err) {
      setResult({
        installed: false,
        version: null,
        authenticated: false,
        error: String(err),
      });
      setState("missing");
    }
  };

  useEffect(() => {
    void runCheck();
  }, []);

  return (
    <StepContainer eyebrow="Step 1" title="Connect Claude Code">
      <p className="text-[14px] text-ink-muted">
        Continuum drives the official <Code>claude</Code> CLI as a subprocess. Let&apos;s confirm
        it&apos;s installed and signed in.
      </p>

      <div className="w-soft">
        <div className="w-row">
          <StatusIcon
            state={state === "checking" ? "pending" : result?.installed ? "ok" : "fail"}
          />
          <span className="flex-1 text-[13px] font-medium text-ink">Claude Code CLI</span>
          <span className="text-[12px] text-ink-dim">
            {result?.installed ? (result.version ?? "installed") : "not found"}
          </span>
        </div>
        <div className="w-row">
          <StatusIcon
            state={
              state === "checking"
                ? "pending"
                : result?.authenticated
                  ? "ok"
                  : result?.installed
                    ? "fail"
                    : "pending"
            }
          />
          <span className="flex-1 text-[13px] font-medium text-ink">Signed in</span>
          <span className="text-[12px] text-ink-dim">
            {result?.authenticated ? "ready" : "run 'claude login'"}
          </span>
        </div>
      </div>

      {state !== "ok" && (
        <button onClick={runCheck} className="press w-fit rounded-lg border border-bg-border px-3 py-1.5 text-[12px] text-ink-muted hover:bg-white/5 hover:text-ink">
          Check again
        </button>
      )}
      {state === "missing" && <Code>npm install -g @anthropic-ai/claude-code</Code>}
      {state === "unauth" && <Code>claude login</Code>}
      {result?.error && (
        <p className="flex items-center gap-2 text-[12px] text-red-400">
          <AlertCircle size={13} /> {result.error}
        </p>
      )}
    </StepContainer>
  );
}

interface ModelInfo {
  key: string;
  label: string;
  size: string;
  purpose: string;
  url: string;
}

const MODELS: ModelInfo[] = [
  { key: "smolvlm", label: "SmolVLM-256M", size: "~500 MB", purpose: "Vision", url: "" },
  { key: "qwen3-8b", label: "Qwen 3 8B Q4_K_M", size: "~4.5 GB", purpose: "Triage", url: "" },
  { key: "whisper-medium", label: "Whisper medium", size: "~1.5 GB", purpose: "Speech-to-text", url: "" },
  { key: "piper-voices", label: "Piper voices", size: "~150 MB", purpose: "Text-to-speech", url: "" },
];

function ModelsStep({ onNext: _onNext }: { onNext: () => void }) {
  const [progress, setProgress] = useState<Record<string, number>>({});
  const [running, setRunning] = useState(false);

  const downloadAll = async () => {
    setRunning(true);
    try {
      await invoke("download_model", { name: "__all__", url: "" });
    } catch (err) {
      console.warn("download_model(__all__) failed, wizard will not block", err);
    } finally {
      setRunning(false);
    }
  };

  useEffect(() => {
    let unsubscribe: (() => void) | undefined;
    (async () => {
      try {
        const event = await import("@tauri-apps/api/event");
        unsubscribe = await event.listen<{ model: string; percent: number }>(
          "continuum:onboarding:progress",
          (e) => setProgress((p) => ({ ...p, [e.payload.model]: e.payload.percent }))
        );
      } catch {
        /* outside Tauri */
      }
    })();
    return () => unsubscribe?.();
  }, []);

  return (
    <StepContainer eyebrow="Step 2" title="Download models">
      <p className="text-[14px] text-ink-muted">
        Four model sets power Continuum. Existing models under <Code>~/.continuum/models</Code> are
        reused automatically.
      </p>

      <div className="w-soft">
        {MODELS.map((m, i) => {
          const pct = progress[m.key] ?? 0;
          const done = pct >= 100;
          return (
            <div key={m.key} className={clsx("w-row", i > 0 && "")}>
              <span className="w-36 text-[13px] font-medium text-ink">{m.label}</span>
              <span className="w-20 text-[11px] text-ink-dim">{m.size}</span>
              <span className="flex-1 text-[12px] text-ink-muted">{m.purpose}</span>
              <div className="h-1 w-24 overflow-hidden rounded-full bg-white/10">
                <div
                  className={clsx("h-full transition-all", done ? "bg-emerald-400" : "bg-amber-400")}
                  style={{ width: `${pct}%` }}
                />
              </div>
              {done ? (
                <Check size={14} className="text-emerald-400" />
              ) : pct > 0 ? (
                <Loader2 size={14} className="animate-spin text-amber-400" />
              ) : (
                <span className="h-3.5 w-3.5" />
              )}
            </div>
          );
        })}
      </div>

      <button
        onClick={downloadAll}
        disabled={running}
        className="press inline-flex items-center gap-2 rounded-lg border border-amber-400/50 bg-amber-400/15 px-4 py-2 text-[13px] font-medium text-amber-200 hover:bg-amber-400/25 disabled:opacity-50"
      >
        {running ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />}
        {running ? "Downloading..." : "Download all"}
      </button>
    </StepContainer>
  );
}

interface AudioDevice {
  name: string;
  id: string;
}

function VoiceStep({
  payload,
  setPayload,
}: {
  payload: OnboardingPayload;
  setPayload: (p: OnboardingPayload) => void;
}) {
  const [mics, setMics] = useState<AudioDevice[]>([]);
  const [speakers, setSpeakers] = useState<AudioDevice[]>([]);

  useEffect(() => {
    (async () => {
      try {
        setMics(await invoke<AudioDevice[]>("list_audio_input_devices"));
        setSpeakers(await invoke<AudioDevice[]>("list_audio_output_devices"));
      } catch {
        /* outside Tauri */
      }
    })();
  }, []);

  return (
    <StepContainer eyebrow="Step 3" title="Voice">
      <div className="w-soft flex flex-col gap-4">
        <ToggleRow
          label="Wake word"
          hint='Say "hey continuum" to talk. Ctrl+Shift+K always works as push-to-talk.'
          checked={payload.wake_word_enabled}
          onChange={(v) => setPayload({ ...payload, wake_word_enabled: v })}
        />
        <div>
          <div className="mb-1 flex items-center justify-between">
            <span className="text-[13px] font-medium text-ink">Sensitivity</span>
            <span className="text-[11px] text-ink-dim">
              {payload.wake_sensitivity.toFixed(2)}
            </span>
          </div>
          <input
            type="range"
            min={0}
            max={1}
            step={0.05}
            value={payload.wake_sensitivity}
            onChange={(e) => setPayload({ ...payload, wake_sensitivity: Number(e.target.value) })}
            className="continuum-range w-full"
          />
          <p className="mt-1 text-[11px] text-ink-dim">
            Higher = fewer false wakes, may miss quiet ones.
          </p>
        </div>
        <div>
          <span className="mb-1.5 block text-[13px] font-medium text-ink">Language</span>
          <div className="flex gap-2">
            {(["en", "nl", "both"] as const).map((l) => (
              <Chip
                key={l}
                active={payload.language === l}
                onClick={() => setPayload({ ...payload, language: l })}
              >
                {l === "en" ? "English" : l === "nl" ? "Dutch" : "Both"}
              </Chip>
            ))}
          </div>
        </div>
      </div>

      <div className="flex flex-col gap-3">
        <DevicePicker
          label="Microphone"
          icon={Mic}
          devices={mics}
          value={payload.mic_device}
          onChange={(d) => setPayload({ ...payload, mic_device: d })}
        />
        <DevicePicker
          label="Speaker"
          icon={Volume2}
          devices={speakers}
          value={payload.speaker_device}
          onChange={(d) => setPayload({ ...payload, speaker_device: d })}
        />
      </div>
    </StepContainer>
  );
}

function ToggleRow({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string;
  hint: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-center justify-between gap-4">
      <div>
        <p className="text-[13px] font-medium text-ink">{label}</p>
        <p className="text-[11px] text-ink-dim">{hint}</p>
      </div>
      <button
        onClick={() => onChange(!checked)}
        className={clsx(
          "press relative h-6 w-11 rounded-full transition-colors",
          checked ? "bg-amber-400" : "bg-white/10"
        )}
      >
        <span
          className={clsx(
            "absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all",
            checked ? "left-[22px]" : "left-0.5"
          )}
        />
      </button>
    </div>
  );
}

function Chip({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className={clsx(
        "press rounded-lg px-3 py-1.5 text-[12px] font-medium transition-colors",
        active
          ? "border border-amber-400/50 bg-amber-400/15 text-amber-200"
          : "border border-bg-border text-ink-muted hover:bg-white/5 hover:text-ink"
      )}
    >
      {children}
    </button>
  );
}

function DevicePicker({
  label,
  icon: Icon,
  devices,
  value,
  onChange,
}: {
  label: string;
  icon: typeof Mic;
  devices: AudioDevice[];
  value: string | undefined;
  onChange: (id: string) => void;
}) {
  return (
    <div>
      <p className="mb-1 flex items-center gap-2 text-[13px] font-medium text-ink">
        <Icon size={14} className="text-amber-400" /> {label}
      </p>
      <select
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value)}
        className="continuum-select w-full rounded-lg border border-bg-border bg-bg-elevated px-3 py-2 text-[13px] text-ink"
      >
        <option value="">System default</option>
        {devices.map((d) => (
          <option key={d.id} value={d.id}>
            {d.name}
          </option>
        ))}
      </select>
    </div>
  );
}

function PermissionsStep({
  payload,
  setPayload,
}: {
  payload: OnboardingPayload;
  setPayload: (p: OnboardingPayload) => void;
}) {
  const [newPath, setNewPath] = useState("");

  return (
    <StepContainer eyebrow="Step 4" title="Permissions">
      <p className="text-[14px] text-ink-muted">
        By default, Continuum reads your home folder (read-only) and writes inside a projects folder
        you pick. Secrets and SSH keys are always blocked.
      </p>

      <div className="flex gap-2">
        <Chip active={payload.permissions === "default"} onClick={() => setPayload({ ...payload, permissions: "default" })}>
          Use defaults
        </Chip>
        <Chip active={payload.permissions === "custom"} onClick={() => setPayload({ ...payload, permissions: "custom" })}>
          Custom folders
        </Chip>
      </div>

      {payload.permissions === "custom" && (
        <div className="w-soft">
          <div className="flex gap-2">
            <input
              type="text"
              value={newPath}
              onChange={(e) => setNewPath(e.target.value)}
              placeholder="C:\Users\you\projects"
              className="flex-1 rounded-lg border border-bg-border bg-bg-elevated px-3 py-1.5 text-[13px] text-ink"
            />
            <button
              onClick={() => {
                if (newPath) {
                  setPayload({ ...payload, extra_paths: [...payload.extra_paths, newPath] });
                  setNewPath("");
                }
              }}
              className="press rounded-lg border border-amber-400/50 bg-amber-400/15 px-3 py-1.5 text-[12px] font-medium text-amber-200 hover:bg-amber-400/25"
            >
              Add
            </button>
          </div>
          {payload.extra_paths.length > 0 && (
            <ul className="mt-2 space-y-1">
              {payload.extra_paths.map((p) => (
                <li key={p} className="flex items-center justify-between text-[12px]">
                  <Code>{p}</Code>
                  <button
                    className="text-red-400 hover:underline"
                    onClick={() =>
                      setPayload({
                        ...payload,
                        extra_paths: payload.extra_paths.filter((x) => x !== p),
                      })
                    }
                  >
                    remove
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <p className="text-[11px] text-ink-dim">
        Always blocked: .ssh, .aws, .gnupg, browser profiles, *.pem / *.key, .env files.
      </p>
    </StepContainer>
  );
}

function PersonalStep({
  payload,
  setPayload,
}: {
  payload: OnboardingPayload;
  setPayload: (p: OnboardingPayload) => void;
}) {
  return (
    <StepContainer eyebrow="Step 5" title="A little about you">
      <p className="text-[14px] text-ink-muted">
        Optional. Stored in Continuum&apos;s semantic memory so the orchestrator can use it. Editable
        later from the Memory tab.
      </p>
      <div className="flex flex-col gap-3">
        <LabeledInput
          label="Name"
          value={payload.name ?? ""}
          onChange={(v) => setPayload({ ...payload, name: v })}
          placeholder="How should Continuum address you?"
        />
        <LabeledInput
          label="Timezone"
          value={payload.timezone ?? ""}
          onChange={(v) => setPayload({ ...payload, timezone: v })}
          placeholder="Europe/Amsterdam"
        />
      </div>
    </StepContainer>
  );
}

function LabeledInput({
  label,
  value,
  onChange,
  placeholder,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
}) {
  return (
    <label className="block">
      <span className="mb-1 block text-[12px] font-medium text-ink-muted">{label}</span>
      <input
        type="text"
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        className="w-full rounded-lg border border-bg-border bg-bg-elevated px-3 py-2 text-[13px] text-ink"
      />
    </label>
  );
}

interface DiagnosticCheck {
  name: string;
  status: "ok" | "fail" | "skip" | "pending";
  detail?: string;
}

function DiagnosticsStep() {
  const [checks, setChecks] = useState<DiagnosticCheck[]>(INITIAL_CHECKS);
  const [running, setRunning] = useState(false);
  const allPass = useMemo(
    () => checks.every((c) => c.status === "ok" || c.status === "skip"),
    [checks]
  );

  const run = async () => {
    setRunning(true);
    setChecks(INITIAL_CHECKS.map((c) => ({ ...c, status: "pending" as const })));
    try {
      const result = await invoke<{ checks: DiagnosticCheck[] }>("run_diagnostics");
      setChecks(result.checks);
    } catch (err) {
      setChecks((cs) =>
        cs.map((c) => ({ ...c, status: "fail" as const, detail: String(err) }))
      );
    } finally {
      setRunning(false);
    }
  };

  useEffect(() => {
    void run();
  }, []);

  return (
    <StepContainer eyebrow="Step 6" title="Diagnostics">
      <p className="text-[14px] text-ink-muted">
        One last check that everything is wired up. Retry, or let the repair agent try.
      </p>

      <div className="w-soft">
        {checks.map((c, i) => (
          <div key={c.name} className={clsx("w-row", i > 0 && "")}>
            <StatusIcon state={c.status} />
            <span className="flex-1 text-[13px] font-medium text-ink">{c.name}</span>
            {c.detail && (
              <span
                className={clsx(
                  "text-[11px]",
                  c.status === "fail" ? "text-red-400" : "text-ink-dim"
                )}
              >
                {c.detail}
              </span>
            )}
          </div>
        ))}
      </div>

      <div className="flex gap-2">
        <button
          onClick={run}
          disabled={running}
          className="press inline-flex items-center gap-2 rounded-lg border border-amber-400/50 bg-amber-400/15 px-3 py-1.5 text-[12px] font-medium text-amber-200 hover:bg-amber-400/25 disabled:opacity-50"
        >
          {running ? <Loader2 size={13} className="animate-spin" /> : <RefreshIcon />}
          {running ? "Running..." : "Re-run"}
        </button>
        {!allPass && !running && (
          <button
            onClick={() => invoke("trigger_repair", { reason: "onboarding-diagnostics" })}
            className="press rounded-lg border border-bg-border px-3 py-1.5 text-[12px] text-ink-muted hover:bg-white/5 hover:text-ink"
          >
            Fix with repair agent
          </button>
        )}
      </div>
    </StepContainer>
  );
}

function RefreshIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
      <path d="M21 12a9 9 0 1 1-3-6.7" />
      <path d="M21 3v6h-6" />
    </svg>
  );
}

const INITIAL_CHECKS: DiagnosticCheck[] = [
  { name: "Claude Code CLI", status: "pending" },
  { name: "Vision model (SmolVLM-256M)", status: "pending" },
  { name: "Triage model (Qwen 3 8B)", status: "pending" },
  { name: "Whisper STT", status: "pending" },
  { name: "Piper TTS", status: "pending" },
  { name: "Microphone capture", status: "pending" },
  { name: "Screen capture", status: "pending" },
  { name: "Memory database", status: "pending" },
];

function DoneStep({ onStart: _onStart }: { onStart: () => void }) {
  return (
    <StepContainer eyebrow="Ready" title="Continuum is set up">
      <p className="text-[14px] text-ink-muted">It runs quietly in the background from now on.</p>
      <ul className="flex flex-col gap-2.5 text-[13px] text-ink-muted">
        <Bullet>
          Say <em>&quot;hey continuum, hello&quot;</em> for the simplest end-to-end test.
        </Bullet>
        <Bullet>
          Press <Code>Ctrl+Shift+K</Code> to talk without the wake word.
        </Bullet>
        <Bullet>Left-click the tray icon to open this dashboard again.</Bullet>
      </ul>
      <p className="text-[11px] text-ink-dim">
        Something wrong? Open the Health tab and click &quot;Fix Issues&quot;.
      </p>
    </StepContainer>
  );
}

// ---- Helpers ---------------------------------------------------------------

function Code({ children }: { children: React.ReactNode }) {
  return (
    <code className="rounded-md border border-bg-border bg-bg-elevated px-1.5 py-0.5 font-mono text-[12px] text-amber-200">
      {children}
    </code>
  );
}

function StatusIcon({ state }: { state: "ok" | "fail" | "skip" | "pending" }) {
  if (state === "ok") return <Check size={16} className="text-emerald-400" />;
  if (state === "fail") return <AlertCircle size={16} className="text-red-400" />;
  if (state === "skip") return <Check size={16} className="text-ink-dim" />;
  return <Loader2 size={16} className="animate-spin text-amber-400" />;
}