"use client";

import { Info, Volume2 } from "lucide-react";

import { useStore } from "@/lib/store";
import { continuum } from "@/lib/tauri";
import { Button, Card, Kbd, Select, Slider, Toggle } from "@/components/ui/primitives";

export function VoiceTab() {
  const voice = useStore((s) => s.state.voice);
  const config = useStore((s) => s.config);
  const setConfig = useStore((s) => s.setConfig);

  const voiceOptions =
    Object.keys(config.tts.voices).length > 0
      ? Object.keys(config.tts.voices).map((k) => ({ value: k, label: k }))
      : [{ value: config.tts.primary || "en", label: config.tts.primary || "en" }];

  return (
    <div className="mx-auto max-w-5xl space-y-6">
      <RestartNotice />

      <Card title="Voice status" subtitle={`mode: ${voice.mode}${voice.muted ? " (muted)" : ""}`}>
        <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
          <Stat label="Volume" value={`${Math.round(voice.volume * 100)}%`} />
          <Stat label="TTS queue" value={String(voice.tts_queue_len)} />
          <Stat
            label="Ambient mute"
            value={voice.ambient_mute_active ? `yes (${voice.detected_call_app ?? "call"})` : "no"}
          />
        </div>
        {voice.partial_transcript && (
          <div className="mt-4 rounded-md border border-bg-border bg-bg-elevated p-3 text-sm">
            <div className="text-[11px] uppercase tracking-wider text-ink-dim">
              partial transcript
            </div>
            <div className="mt-1 text-ink">"{voice.partial_transcript}"</div>
          </div>
        )}
      </Card>

      <Card title="Wake word">
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
          <Toggle
            checked={config.voice.wake_word_enabled}
            onChange={async (v) => {
              const cfg = await continuum.updateVoiceFlag("wake_word_enabled", v);
              setConfig(cfg);
            }}
            label="Wake word enabled"
          />
          {/* `wake_sensitivity` is currently not consumed by the wake-word
              detector (transcript-based phonetic match has no threshold).
              Hidden until that field is actually wired into the matcher. */}
        </div>
        <div className="mt-4 text-sm text-ink-muted">
          Hotkey: <Kbd>{config.voice.hotkey || "unset"}</Kbd>
          <span className="ml-2 text-xs text-ink-dim">
            (rebind via config.toml - UI rebind coming later)
          </span>
        </div>
      </Card>

      <Card title="Text to speech">
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
          <Select
            label="Engine"
            value={config.tts.engine}
            options={[
              { value: "piper", label: "Piper (local)" },
              { value: "elevenlabs", label: "ElevenLabs (cloud plugin)" },
            ]}
            onChange={async (v) => {
              const cfg = await continuum.updateTtsEngine(v);
              setConfig(cfg);
            }}
          />
          <Select
            label="Primary voice"
            value={config.tts.primary}
            options={voiceOptions}
            onChange={async (v) => {
              const cfg = await continuum.updateTtsPrimaryVoice(v);
              setConfig(cfg);
            }}
          />
          <Slider
            label="Volume"
            value={config.voice.volume}
            onChange={async (v) => {
              const cfg = await continuum.updateVoiceVolume(v);
              setConfig(cfg);
            }}
            format={(v) => `${Math.round(v * 100)}%`}
          />
          <Slider
            label="Length scale (speed)"
            value={config.tts.length_scale ?? 1}
            onChange={async (v) => {
              const cfg = await continuum.updateTtsLengthScale(v);
              setConfig(cfg);
            }}
            min={0.7}
            max={1.4}
            step={0.05}
          />
        </div>
        <div className="mt-4 flex items-center gap-2">
          <Button size="sm" variant="default">
            <Volume2 size={12} /> Preview
          </Button>
          <span className="text-xs text-ink-muted">
            Plays "Continuum checking in" using the primary voice.
          </span>
        </div>
      </Card>

      <Card title="Behaviour">
        <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
          <Toggle
            checked={config.voice.barge_in_enabled}
            onChange={async (v) => {
              const cfg = await continuum.updateVoiceFlag("barge_in_enabled", v);
              setConfig(cfg);
            }}
            label="Barge-in (stop speaking when I talk)"
          />
          <Toggle
            checked={config.voice.ambient_mute_enabled}
            onChange={async (v) => {
              const cfg = await continuum.updateVoiceFlag("ambient_mute_enabled", v);
              setConfig(cfg);
            }}
            label="Mute during calls"
          />
          <Toggle
            checked={config.voice.feedback_sounds}
            onChange={async (v) => {
              const cfg = await continuum.updateVoiceFlag("feedback_sounds", v);
              setConfig(cfg);
            }}
            label="Feedback sounds (chimes)"
          />
          <Toggle
            checked={config.voice.language_detection_enabled}
            onChange={async (v) => {
              const cfg = await continuum.updateVoiceFlag("language_detection_enabled", v);
              setConfig(cfg);
            }}
            label="Auto-switch voice by detected language"
          />
        </div>
      </Card>
    </div>
  );
}

/**
 * Banner that warns the user voice settings only take effect on the next
 * daemon start. The daemon currently loads its config once at boot and does
 * not watch `config.toml` for changes - making the dashboard's slider/select
 * changes "save now, apply on restart". Honest UX beats silent no-op.
 */
function RestartNotice() {
  return (
    <div className="flex items-start gap-3 rounded-md border border-state-warn/40 bg-state-warn/10 p-3 text-sm">
      <Info size={16} className="mt-0.5 shrink-0 text-state-warn" />
      <div>
        <div className="font-medium text-ink">
          Voice settings will only take effect on the next Continuum start.
        </div>
        <div className="mt-0.5 text-xs text-ink-muted">
          Changes here are saved, but the daemon only reads the config at startup. Close and restart
          Continuum to apply them. (Live hot-reload will ship in a later update.)
        </div>
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-[11px] uppercase tracking-wider text-ink-dim">{label}</div>
      <div className="mt-1 text-sm text-ink">{value}</div>
    </div>
  );
}
