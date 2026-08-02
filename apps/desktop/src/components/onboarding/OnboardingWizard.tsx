// OnboardingWizard — eight-step first-run flow for Continuum.
//
// Mounted by Shell when `is_onboarding_complete` returns false. Uses the same
// UI primitives and dark palette as the rest of the dashboard. Each step is a
// focused panel; Next / Back navigation is handled by a single local state
// machine.
//
// Backend contract (apps/desktop/src-tauri/src/commands.rs):
//   check_claude_cli()         -> { installed: bool, version: string | null, error: string | null }
//   check_claude_auth()        -> { authenticated: bool, error: string | null }
//   list_audio_input_devices() -> [{ name, id }]
//   list_audio_output_devices()-> [{ name, id }]
//   list_tts_voices()          -> [{ id, language, path }]
//   download_model(name, url)  -> starts a download; progress via `continuum:onboarding:progress` event
//   run_diagnostics()          -> { checks: [{ name, status: "ok" | "fail" | "skip", detail }] }
//   is_onboarding_complete()   -> bool
//   complete_onboarding(payload) -> void  // persists ~/.continuum/config/onboarding-complete

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
} from "lucide-react";
import { clsx } from "clsx";
import { invoke } from "@tauri-apps/api/core";

type StepId =
  "welcome" | "claude" | "models" | "voice" | "permissions" | "personal" | "diagnostics" | "done";

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

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-bg">
      <div className="flex h-full w-full max-w-5xl flex-col">
        <WizardHeader idx={idx} />
        <div className="flex-1 overflow-y-auto px-8 py-8">
          {step === "welcome" && <WelcomeStep onNext={goNext} />}
          {step === "claude" && <ClaudeStep onNext={goNext} />}
          {step === "models" && <ModelsStep onNext={goNext} />}
          {step === "voice" && <VoiceStep payload={payload} setPayload={setPayload} />}
          {step === "permissions" && <PermissionsStep payload={payload} setPayload={setPayload} />}
          {step === "personal" && <PersonalStep payload={payload} setPayload={setPayload} />}
          {step === "diagnostics" && <DiagnosticsStep />}
          {step === "done" && <DoneStep onStart={finish} />}
        </div>
        <WizardFooter
          idx={idx}
          onBack={goBack}
          onNext={step === "done" ? finish : goNext}
          nextLabel={step === "done" ? "Start Continuum" : "Next"}
          canBack={idx > 0 && step !== "done"}
        />
      </div>
    </div>
  );
}

function detectLanguage(): "en" | "nl" | "both" {
  if (typeof navigator === "undefined") return "en";
  const lang = navigator.language.toLowerCase();
  if (lang.startsWith("nl")) return "both";
  return "en";
}

function WizardHeader({ idx }: { idx: number }) {
  return (
    <header className="flex items-center gap-4 border-b border-bg-border bg-bg-surface px-8 py-4">
      <span className="text-lg font-semibold">
        K<span className="text-accent-purple">AI</span>ro
        <span className="ml-3 text-sm font-normal text-ink-dim">first-run setup</span>
      </span>
      <div className="ml-auto flex items-center gap-1">
        {STEPS.map((s, i) => (
          <span
            key={s.id}
            title={s.label}
            className={clsx(
              "h-1.5 w-8 rounded-full transition-colors",
              i < idx && "bg-accent-purple",
              i === idx && "bg-accent-purple",
              i > idx && "bg-bg-border"
            )}
          />
        ))}
      </div>
    </header>
  );
}

function WizardFooter({
  idx,
  onBack,
  onNext,
  nextLabel,
  canBack,
}: {
  idx: number;
  onBack: () => void;
  onNext: () => void;
  nextLabel: string;
  canBack: boolean;
}) {
  return (
    <footer className="flex items-center justify-between border-t border-bg-border bg-bg-surface px-8 py-4">
      <button
        disabled={!canBack}
        onClick={onBack}
        className={clsx(
          "inline-flex items-center gap-1.5 rounded-md border border-bg-border px-3 py-1.5 text-sm",
          canBack ? "hover:bg-bg-hover" : "cursor-not-allowed opacity-40"
        )}
      >
        <ChevronLeft size={14} /> Back
      </button>
      <span className="text-xs text-ink-dim">
        Step {idx + 1} of {STEPS.length}
      </span>
      <button
        onClick={onNext}
        className="inline-flex items-center gap-1.5 rounded-md bg-accent-purple px-4 py-1.5 text-sm text-white hover:opacity-90"
      >
        {nextLabel} <ChevronRight size={14} />
      </button>
    </footer>
  );
}

// ---- Steps ------------------------------------------------------------------

function WelcomeStep({ onNext: _onNext }: { onNext: () => void }) {
  return (
    <StepContainer title="Welcome to Continuum">
      <p className="text-ink-muted">
        Continuum is an ambient AI assistant for Windows. It sees what you see, hears what you hear,
        remembers what matters, and acts only when the moment is right — powered by Claude Code and
        small local models.
      </p>
      <p className="text-ink-muted">
        This wizard takes about ten minutes. Most of that is a one-time model download that runs in
        the background while you configure the rest. You can rerun it later with{" "}
        <code className="continuum-code">continuum setup</code>.
      </p>
      <div className="continuum-card">
        <h3 className="font-semibold">Before we start</h3>
        <ul className="mt-2 list-disc space-y-1 pl-5 text-sm text-ink-muted">
          <li>Make sure you have a Claude Max or API subscription.</li>
          <li>Have a working microphone and speaker ready (optional but recommended).</li>
          <li>Expect ~6.5 GB of model downloads.</li>
          <li>Nothing you enter is uploaded anywhere — Continuum is local-first.</li>
        </ul>
      </div>
    </StepContainer>
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
    <StepContainer title="Claude Code check">
      <p className="text-ink-muted">
        Continuum drives the official <code className="continuum-code">claude</code> CLI as a subprocess.
        Let&apos;s make sure it&apos;s installed and signed in.
      </p>

      <div className="continuum-card">
        <StatusRow
          label="Claude Code CLI installed"
          state={state === "checking" ? "pending" : result?.installed ? "ok" : "fail"}
          detail={
            result?.installed
              ? (result?.version ?? "")
              : "Run npm install -g @anthropic-ai/claude-code"
          }
        />
        <StatusRow
          label="Logged in"
          state={
            state === "checking"
              ? "pending"
              : result?.authenticated
                ? "ok"
                : result?.installed
                  ? "fail"
                  : "pending"
          }
          detail={result?.authenticated ? "OK" : "Run 'claude login' in a separate terminal"}
        />
      </div>

      {state !== "ok" && (
        <div className="flex gap-3">
          <button onClick={runCheck} className="continuum-button-primary">
            Check again
          </button>
          {state === "missing" && (
            <code className="continuum-code flex items-center px-3">
              npm install -g @anthropic-ai/claude-code
            </code>
          )}
          {state === "unauth" && (
            <code className="continuum-code flex items-center px-3">claude login</code>
          )}
        </div>
      )}

      {result?.error && (
        <div className="continuum-error">
          <AlertCircle size={14} className="inline" /> {result.error}
        </div>
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
  {
    key: "smolvlm",
    label: "SmolVLM-256M",
    size: "~500 MB",
    purpose: "Vision (screen description)",
    url: "https://huggingface.co/HuggingFaceTB/SmolVLM-256M-Instruct",
  },
  {
    key: "qwen3-8b",
    label: "Qwen 3 8B Q4_K_M",
    size: "~4.5 GB",
    purpose: "Triage (local decision LLM)",
    url: "https://huggingface.co/Qwen/Qwen3-8B-GGUF",
  },
  {
    key: "whisper-medium",
    label: "Whisper medium",
    size: "~1.5 GB",
    purpose: "Speech-to-text",
    url: "https://huggingface.co/ggerganov/whisper.cpp",
  },
  {
    key: "piper-voices",
    label: "Piper voices (en + binary)",
    size: "~150 MB",
    purpose: "Text-to-speech",
    url: "https://github.com/rhasspy/piper",
  },
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
    // Subscribe to download progress if the Tauri layer is available.
    let unsubscribe: (() => void) | undefined;
    (async () => {
      try {
        const event = await import("@tauri-apps/api/event");
        unsubscribe = await event.listen<{ model: string; percent: number }>(
          "continuum:onboarding:progress",
          (e) => setProgress((p) => ({ ...p, [e.payload.model]: e.payload.percent }))
        );
      } catch {
        // Running outside Tauri — no live progress, which is fine.
      }
    })();
    return () => unsubscribe?.();
  }, []);

  return (
    <StepContainer title="Download models">
      <p className="text-ink-muted">
        Continuum needs four sets of models to work. If you&apos;ve installed Continuum before, existing
        models under <code className="continuum-code">~/.continuum/models</code> will be reused.
      </p>

      <div className="continuum-card space-y-2">
        {MODELS.map((m) => {
          const pct = progress[m.key] ?? 0;
          const done = pct >= 100;
          return (
            <div key={m.key} className="flex items-center gap-3 py-1">
              <span className="w-52 font-medium">{m.label}</span>
              <span className="w-28 text-xs text-ink-dim">{m.size}</span>
              <span className="flex-1 text-xs text-ink-muted">{m.purpose}</span>
              <div className="h-1.5 w-32 overflow-hidden rounded-full bg-bg-border">
                <div
                  className={clsx("h-full", done ? "bg-green-500" : "bg-accent-purple")}
                  style={{ width: `${pct}%` }}
                />
              </div>
              {done ? (
                <Check size={14} className="text-green-500" />
              ) : pct > 0 ? (
                <Loader2 size={14} className="animate-spin text-accent-purple" />
              ) : (
                <span className="h-3.5 w-3.5" />
              )}
            </div>
          );
        })}
      </div>

      <div className="flex gap-3">
        <button onClick={downloadAll} disabled={running} className="continuum-button-primary">
          {running ? "Downloading..." : "Download all"}
        </button>
        <span className="text-xs text-ink-dim">
          You can skip this step and run <code className="continuum-code">continuum setup</code> later.
        </span>
      </div>
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
        // Running outside Tauri.
      }
    })();
  }, []);

  return (
    <StepContainer title="Voice setup">
      <div className="continuum-card space-y-4">
        <div className="flex items-center justify-between">
          <div>
            <p className="font-medium">Wake word</p>
            <p className="text-xs text-ink-muted">
              Default phrase: &quot;hey continuum&quot;. You can always use Ctrl+Shift+K as a
              push-to-talk.
            </p>
          </div>
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={payload.wake_word_enabled}
              onChange={(e) => setPayload({ ...payload, wake_word_enabled: e.target.checked })}
            />
            <span className="text-sm">Enable</span>
          </label>
        </div>

        <div>
          <p className="font-medium">Sensitivity</p>
          <input
            type="range"
            min={0}
            max={1}
            step={0.05}
            value={payload.wake_sensitivity}
            onChange={(e) => setPayload({ ...payload, wake_sensitivity: Number(e.target.value) })}
            className="mt-1 w-full"
          />
          <p className="text-xs text-ink-dim">
            Higher = stricter (fewer false positives, more missed wakes). Current:{" "}
            {payload.wake_sensitivity.toFixed(2)}
          </p>
        </div>

        <div>
          <p className="font-medium">Language</p>
          <div className="mt-1 flex gap-4 text-sm">
            {(["en", "nl", "both"] as const).map((l) => (
              <label key={l} className="flex items-center gap-1">
                <input
                  type="radio"
                  checked={payload.language === l}
                  onChange={() => setPayload({ ...payload, language: l })}
                />
                <span>{l === "en" ? "English" : l === "nl" ? "Dutch" : "Both"}</span>
              </label>
            ))}
          </div>
          <p className="text-xs text-ink-dim">
            Whisper handles input in any language. Output is English by default — Dutch TTS voice is
            available but lower quality.
          </p>
        </div>
      </div>

      <div className="continuum-card space-y-3">
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
      <p className="flex items-center gap-2 font-medium">
        <Icon size={14} /> {label}
      </p>
      <select
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value)}
        className="mt-1 w-full rounded-md border border-bg-border bg-bg-elevated px-2 py-1 text-sm"
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
    <StepContainer title="Permissions">
      <p className="text-ink-muted">
        Decide where Continuum can read and write. By default, it has read-only access to your home
        directory and read-write in a projects folder you pick. Secrets, SSH keys, and browser
        profiles are always blocked.
      </p>

      <div className="continuum-card">
        <p className="font-medium">Mode</p>
        <div className="mt-2 flex gap-4 text-sm">
          {(["default", "custom"] as const).map((m) => (
            <label key={m} className="flex items-center gap-1">
              <input
                type="radio"
                checked={payload.permissions === m}
                onChange={() => setPayload({ ...payload, permissions: m })}
              />
              <span>{m === "default" ? "I'll use defaults" : "Custom folders"}</span>
            </label>
          ))}
        </div>
      </div>

      {payload.permissions === "custom" && (
        <div className="continuum-card">
          <p className="font-medium">Additional read-write paths</p>
          <div className="mt-2 flex gap-2">
            <input
              type="text"
              value={newPath}
              onChange={(e) => setNewPath(e.target.value)}
              placeholder="C:\Users\you\projects"
              className="flex-1 rounded-md border border-bg-border bg-bg-elevated px-2 py-1 text-sm"
            />
            <button
              onClick={() => {
                if (newPath) {
                  setPayload({
                    ...payload,
                    extra_paths: [...payload.extra_paths, newPath],
                  });
                  setNewPath("");
                }
              }}
              className="continuum-button-primary"
            >
              Add
            </button>
          </div>
          <ul className="mt-2 space-y-1 text-sm text-ink-muted">
            {payload.extra_paths.map((p) => (
              <li key={p} className="flex justify-between">
                <code className="continuum-code">{p}</code>
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
        </div>
      )}

      <div className="continuum-card">
        <p className="font-medium">Always blocked</p>
        <p className="mt-1 text-xs text-ink-muted">
          These paths Continuum never reads or writes, regardless of config:
        </p>
        <ul className="mt-2 grid grid-cols-2 gap-1 text-xs text-ink-dim">
          {[
            ".ssh",
            ".aws",
            ".gnupg",
            ".docker",
            "User Data (browsers)",
            "Profiles",
            "*.pem / *.key / id_rsa*",
            ".env*",
            "*.kdbx (KeePass)",
            "AppData (by default)",
          ].map((d) => (
            <li key={d}>
              <code className="continuum-code">{d}</code>
            </li>
          ))}
        </ul>
      </div>
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
    <StepContainer title="A little about you">
      <p className="text-ink-muted">
        Anything you enter here gets written to Continuum&apos;s semantic memory so the orchestrator can
        use it. Everything is optional and editable later from the Memory tab.
      </p>

      <div className="continuum-card space-y-3">
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
      <span className="text-sm font-medium">{label}</span>
      <input
        type="text"
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        className="mt-1 w-full rounded-md border border-bg-border bg-bg-elevated px-3 py-1.5 text-sm"
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
        cs.map((c) => ({
          ...c,
          status: "fail" as const,
          detail: String(err),
        }))
      );
    } finally {
      setRunning(false);
    }
  };

  useEffect(() => {
    void run();
  }, []);

  return (
    <StepContainer title="Diagnostics">
      <p className="text-ink-muted">
        One last check that everything is wired up correctly. If anything fails, you can retry or
        let the repair agent attempt a fix.
      </p>

      <div className="continuum-card space-y-1">
        {checks.map((c) => (
          <StatusRow key={c.name} label={c.name} state={c.status} detail={c.detail} />
        ))}
      </div>

      <div className="flex gap-3">
        <button onClick={run} disabled={running} className="continuum-button-primary">
          {running ? "Running..." : "Re-run"}
        </button>
        {!allPass && !running && (
          <button
            onClick={() => invoke("trigger_repair", { reason: "onboarding-diagnostics" })}
            className="rounded-md border border-accent-purple px-3 py-1.5 text-sm text-accent-purple hover:bg-bg-hover"
          >
            Fix with repair agent
          </button>
        )}
      </div>
    </StepContainer>
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
    <StepContainer title="Continuum is ready">
      <p className="text-ink-muted">
        That&apos;s it. Continuum will run in the background from now on.
      </p>
      <div className="continuum-card space-y-2 text-sm text-ink-muted">
        <p className="font-medium text-ink">A few things to try first:</p>
        <ul className="list-disc space-y-1 pl-5">
          <li>
            Say <em>&quot;hey continuum, hello&quot;</em> — simplest end-to-end test.
          </li>
          <li>
            Press <code className="continuum-code">Ctrl+Shift+K</code> to talk without the wake word.
          </li>
          <li>Left-click the tray icon to open this dashboard again.</li>
          <li>Right-click the tray icon for pause / mute / quit.</li>
        </ul>
      </div>
      <p className="text-xs text-ink-dim">
        Found something wrong? Head to the Health tab and click &quot;Fix Issues&quot; — the repair
        agent will diagnose and try to fix it.
      </p>
    </StepContainer>
  );
}

// ---- Helpers ---------------------------------------------------------------

function StepContainer({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-4">
      <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
      {children}
    </div>
  );
}

function StatusRow({
  label,
  state,
  detail,
}: {
  label: string;
  state: "ok" | "fail" | "skip" | "pending";
  detail?: string;
}) {
  return (
    <div className="flex items-center gap-3 py-1">
      <StatusIcon state={state} />
      <span className="flex-1 font-medium">{label}</span>
      {detail && (
        <span className={clsx("text-xs", state === "fail" ? "text-red-400" : "text-ink-dim")}>
          {detail}
        </span>
      )}
    </div>
  );
}

function StatusIcon({ state }: { state: "ok" | "fail" | "skip" | "pending" }) {
  if (state === "ok") return <Check size={16} className="text-green-500" />;
  if (state === "fail") return <AlertCircle size={16} className="text-red-400" />;
  if (state === "skip") return <Check size={16} className="text-ink-dim" />;
  return <Loader2 size={16} className="animate-spin text-accent-purple" />;
}
