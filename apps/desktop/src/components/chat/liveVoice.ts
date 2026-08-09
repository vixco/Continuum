"use client";

/**
 * Conversational voice loop for the Chat tab.
 *
 * One click starts a session: listen -> final transcript -> model turn ->
 * speak response -> listen again. Every unsupported/denied/error state is
 * surfaced through callbacks instead of becoming a silent no-op.
 */

export type LiveVoicePhase =
  | "idle"
  | "starting"
  | "listening"
  | "thinking"
  | "speaking"
  | "error";

interface RecognitionAlternativeLike {
  transcript: string;
}

interface RecognitionResultLike {
  isFinal: boolean;
  length: number;
  [index: number]: RecognitionAlternativeLike;
}

interface RecognitionResultListLike {
  length: number;
  [index: number]: RecognitionResultLike;
}

interface RecognitionEventLike extends Event {
  resultIndex: number;
  results: RecognitionResultListLike;
}

interface RecognitionErrorEventLike extends Event {
  error: string;
  message?: string;
}

interface RecognitionLike {
  continuous: boolean;
  interimResults: boolean;
  maxAlternatives: number;
  lang: string;
  onstart: (() => void) | null;
  onresult: ((event: RecognitionEventLike) => void) | null;
  onerror: ((event: RecognitionErrorEventLike) => void) | null;
  onend: (() => void) | null;
  onspeechend: (() => void) | null;
  start(): void;
  stop(): void;
  abort(): void;
}

type RecognitionCtor = new () => RecognitionLike;

interface LiveVoiceCallbacks {
  onPhase: (phase: LiveVoicePhase) => void;
  onPartial: (text: string) => void;
  onFinal: (text: string) => void;
  onError: (message: string) => void;
}

interface VoiceWindow {
  SpeechRecognition?: RecognitionCtor;
  webkitSpeechRecognition?: RecognitionCtor;
}

const MAX_TTS_CHUNK = 220;

export class LiveVoiceSession {
  private recognition: RecognitionLike | null = null;
  private stopped = false;
  private generation = 0;
  private finalText = "";
  private interimText = "";
  private recognitionFailed = false;
  private speakingGeneration = 0;

  constructor(private readonly callbacks: LiveVoiceCallbacks) {}

  async start(): Promise<void> {
    this.stopped = false;
    this.callbacks.onPhase("starting");
    this.callbacks.onPartial("");

    const ctor = this.recognitionCtor();
    if (!ctor) {
      this.fail(
        "Live speech recognition is not available in this WebView. Update Microsoft Edge WebView2/Continuum, or use a build with speech recognition support."
      );
      return;
    }

    if (!navigator.mediaDevices?.getUserMedia) {
      this.fail("Microphone access is not available in this desktop WebView.");
      return;
    }

    // Permission/device preflight. SpeechRecognition owns the real capture;
    // close this stream immediately so two microphone readers never stay open.
    try {
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
        },
      });
      for (const track of stream.getTracks()) track.stop();
    } catch (error) {
      this.fail(microphoneError(error));
      return;
    }

    if (!this.stopped) this.startListening();
  }

  thinking(text: string): void {
    if (this.stopped) return;
    this.stopRecognition(false);
    this.callbacks.onPartial(text);
    this.callbacks.onPhase("thinking");
  }

  /** Speak a completed assistant response, then automatically listen again. */
  speak(text: string): void {
    if (this.stopped) return;
    this.stopRecognition(false);

    if (!("speechSynthesis" in window) || typeof SpeechSynthesisUtterance === "undefined") {
      this.fail("Text-to-speech is not available in this desktop WebView.");
      return;
    }

    const speakable = toSpeakableText(text);
    if (!speakable) {
      this.callbacks.onPartial("");
      this.startListening();
      return;
    }

    const chunks = chunkSpeech(speakable);
    const generation = ++this.speakingGeneration;
    let index = 0;

    this.callbacks.onPartial("");
    this.callbacks.onPhase("speaking");
    window.speechSynthesis.cancel();

    const speakNext = () => {
      if (this.stopped || generation !== this.speakingGeneration) return;
      if (index >= chunks.length) {
        this.startListening();
        return;
      }

      const utterance = new SpeechSynthesisUtterance(chunks[index++]);
      const locale = navigator.language || "en-US";
      utterance.lang = locale;
      utterance.rate = 1.03;
      const base = locale.split("-")[0].toLowerCase();
      const voices = window.speechSynthesis.getVoices();
      const preferred =
        voices.find((voice) => voice.lang.toLowerCase() === locale.toLowerCase()) ??
        voices.find((voice) => voice.lang.toLowerCase().startsWith(`${base}-`)) ??
        voices.find((voice) => voice.default);
      if (preferred) utterance.voice = preferred;

      utterance.onend = speakNext;
      utterance.onerror = (event) => {
        const detail = event.error ? ` (${event.error})` : "";
        this.fail(`Voice playback failed${detail}.`);
      };
      window.speechSynthesis.speak(utterance);
    };

    speakNext();
  }

  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    this.generation += 1;
    this.speakingGeneration += 1;
    this.stopRecognition(true);
    if ("speechSynthesis" in window) window.speechSynthesis.cancel();
    this.callbacks.onPartial("");
    this.callbacks.onPhase("idle");
  }

  async retry(): Promise<void> {
    this.stopRecognition(true);
    if ("speechSynthesis" in window) window.speechSynthesis.cancel();
    await this.start();
  }

  private startListening(): void {
    if (this.stopped) return;

    const ctor = this.recognitionCtor();
    if (!ctor) {
      this.fail("Live speech recognition became unavailable in this WebView.");
      return;
    }

    this.stopRecognition(true);
    const generation = ++this.generation;
    const recognition = new ctor();
    this.recognition = recognition;
    this.finalText = "";
    this.interimText = "";
    this.recognitionFailed = false;

    recognition.continuous = false;
    recognition.interimResults = true;
    recognition.maxAlternatives = 1;
    recognition.lang = navigator.language || "en-US";

    recognition.onstart = () => {
      if (this.stopped || generation !== this.generation) return;
      this.callbacks.onPartial("");
      this.callbacks.onPhase("listening");
    };

    recognition.onresult = (event) => {
      if (this.stopped || generation !== this.generation) return;
      let interim = "";
      let final = this.finalText;
      for (let i = event.resultIndex; i < event.results.length; i += 1) {
        const result = event.results[i];
        const text = result[0]?.transcript?.trim() ?? "";
        if (!text) continue;
        if (result.isFinal) final = `${final} ${text}`.trim();
        else interim = `${interim} ${text}`.trim();
      }
      this.finalText = final;
      this.interimText = interim;
      this.callbacks.onPartial(`${final} ${interim}`.trim());
    };

    recognition.onspeechend = () => {
      if (this.stopped || generation !== this.generation) return;
      try {
        recognition.stop();
      } catch {
        // Some implementations already stopped after speechend.
      }
    };

    recognition.onerror = (event) => {
      if (this.stopped || generation !== this.generation) return;
      if (event.error === "aborted") return;
      this.recognitionFailed = true;
      this.fail(recognitionError(event));
    };

    recognition.onend = () => {
      if (this.stopped || generation !== this.generation || this.recognitionFailed) return;
      const final = (this.finalText || this.interimText).trim();
      this.recognition = null;
      if (!final) {
        this.fail("No speech was detected. Check your microphone and try again.");
        return;
      }
      this.callbacks.onPartial(final);
      this.callbacks.onFinal(final);
    };

    try {
      recognition.start();
    } catch (error) {
      this.fail(`Could not start speech recognition: ${errorMessage(error)}`);
    }
  }

  private stopRecognition(abort: boolean): void {
    const recognition = this.recognition;
    this.recognition = null;
    if (!recognition) return;
    try {
      if (abort) recognition.abort();
      else recognition.stop();
    } catch {
      // Already stopped is fine; generation guards ignore stale events.
    }
  }

  private recognitionCtor(): RecognitionCtor | null {
    const voiceWindow = window as unknown as VoiceWindow;
    return voiceWindow.SpeechRecognition ?? voiceWindow.webkitSpeechRecognition ?? null;
  }

  private fail(message: string): void {
    if (this.stopped) return;
    this.stopRecognition(true);
    if ("speechSynthesis" in window) window.speechSynthesis.cancel();
    this.callbacks.onError(message);
    this.callbacks.onPhase("error");
  }
}

function recognitionError(event: RecognitionErrorEventLike): string {
  switch (event.error) {
    case "not-allowed":
    case "service-not-allowed":
      return "Microphone or speech-recognition permission was denied. Allow microphone access for Continuum and retry.";
    case "audio-capture":
      return "No usable microphone was found. Check the Windows input device and retry.";
    case "network":
      return "The speech-recognition service is unreachable. Check your connection and retry.";
    case "no-speech":
      return "No speech was detected. Check your microphone level and retry.";
    case "language-not-supported":
      return `Speech recognition does not support ${navigator.language || "the current language"}.`;
    default:
      return event.message
        ? `Speech recognition failed: ${event.message}`
        : `Speech recognition failed (${event.error || "unknown error"}).`;
  }
}

function microphoneError(error: unknown): string {
  if (error instanceof DOMException) {
    if (error.name === "NotAllowedError" || error.name === "SecurityError") {
      return "Microphone permission was denied. Allow microphone access for Continuum and retry.";
    }
    if (error.name === "NotFoundError" || error.name === "DevicesNotFoundError") {
      return "No microphone was found. Select a Windows input device and retry.";
    }
    if (error.name === "NotReadableError" || error.name === "TrackStartError") {
      return "The microphone is busy or could not be opened. Close other exclusive audio apps and retry.";
    }
  }
  return `Could not open the microphone: ${errorMessage(error)}`;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error || "unknown error");
}

function toSpeakableText(text: string): string {
  return text
    .replace(/```[\s\S]*?```/g, " I added the code in the chat. ")
    .replace(/`([^`]+)`/g, "$1")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, "")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/^#{1,6}\s+/gm, "")
    .replace(/^[-*+]\s+/gm, "")
    .replace(/^>\s?/gm, "")
    .replace(/[*_~]/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

function chunkSpeech(text: string): string[] {
  const sentences = text.match(/[^.!?]+[.!?]+|[^.!?]+$/g) ?? [text];
  const chunks: string[] = [];
  let current = "";

  for (const sentence of sentences) {
    const clean = sentence.trim();
    if (!clean) continue;
    if (!current) {
      current = clean;
      continue;
    }
    if (`${current} ${clean}`.length <= MAX_TTS_CHUNK) current = `${current} ${clean}`;
    else {
      chunks.push(current);
      current = clean;
    }
  }
  if (current) chunks.push(current);

  return chunks.flatMap((chunk) => {
    if (chunk.length <= MAX_TTS_CHUNK) return [chunk];
    const parts: string[] = [];
    for (let i = 0; i < chunk.length; i += MAX_TTS_CHUNK) {
      parts.push(chunk.slice(i, i + MAX_TTS_CHUNK));
    }
    return parts;
  });
}

export { chunkSpeech, microphoneError, recognitionError, toSpeakableText };
