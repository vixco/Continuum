//! # Moshi full-duplex speech-to-speech front-end
//!
//! Kyutai Moshi run as a `moshi-backend.exe` subprocess, driven over its
//! standalone WebSocket protocol. This module is the S2S counterpart to the
//! existing [`super::frontend::PipelineFrontend`] and implements
//! [`super::frontend::VoiceFrontend`].
//!
//! ## Protocol (verified against `kyutai-labs/moshi` `main`)
//!
//! Source of truth: `rust/protocol.md`, `rust/moshi-backend/src/standalone.rs`
//! + `stream_both.rs`, and the reference client `rust/moshi-cli/src/multistream.rs`.
//!
//! - **Endpoint**: `wss://<host>:<port>/api/chat`. The server always serves
//!   `wss://` (TLS) with a self-signed `localhost` cert (auto-generated via
//!   `rcgen` if `cert.pem`/`key.pem` are absent). The client MUST accept
//!   invalid certs.
//! - **Wire framing**: every WebSocket message is **binary**, prefixed with a
//!   single message-type byte ([`MsgType`]):
//!   - `0x00` Handshake: server sends `0x00 + 8 zero bytes` first; discard.
//!   - `0x01` Audio: OGG pages of Opus-encoded audio (24 kHz mono). The
//!     client's first audio frame must contain OpusHead + OpusTags OGG
//!     header pages. **Audio is not raw PCM.**
//!   - `0x02` Text: UTF-8 assistant text deltas (no JSON, no role, no turn
//!     markers). The standalone backend does **not** transcribe user audio
//!     — user text for triage comes from the parallel whisper STT path.
//!   - `0x03` Control: 1-byte payload (Start=0, EndTurn=1, Pause=2,
//!     Restart=3). Ignored by the standalone server in streaming mode;
//!     barge-in is implicit. We send `EndTurn` as a best-effort hint.
//!   - `0x04` MetaData: JSON sent right after handshake; discard.
//!   - `0x05` Error: UTF-8 error string.
//!   - `0x06` Ping: unused.
//! - **Audio codec**: Opus (Voip, 24 kHz mono, 960-sample / 40 ms frames) in
//!   OGG, behind the `moshi-opus` cargo feature (requires libopus at build).
//!   Without it, transport + text + control still work; audio I/O returns a
//!   configured error (see [`MoshiAudioCodec`]).
//! - **Server timeout**: 360 s per connection. Long sessions reconnect.
//!
//! ## What this module does NOT do
//!
//! - Opus/OGG encode+decode lives behind the `moshi-opus` cargo feature and
//!   is implemented in [`OpusOggCodec`] by translating the reference client
//!   (`rust/moshi-cli/src/multistream.rs` send/receive audio arms) and the
//!   backend header layout (`rust/moshi-backend/src/audio.rs`). The base
//!   `moshi` feature (transport + text + control) compiles without libopus;
//!   `moshi-opus` additionally requires libopus at build time (vcpkg `opus`
//!   or the `opus` crate's system-lib path). Without `moshi-opus`, audio
//!   I/O returns a configured error ([`bail_codec`]) so the dashboard /
//!   repair agent can tell the user to enable the feature + install libopus.
//! - Tier-split escalation to the orchestrator (Phase 3) lives in
//!   `bin/continuum.rs`. This module exposes [`MoshiEvent`]s for routing.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use super::frontend::VoiceFrontend;

const TAG: &str = "moshi";

/// Moshi WebSocket message-type byte (first byte of every binary frame).
///
/// Mirrors `rust/moshi-backend/src/stream_both.rs::MsgType`. Values are the
/// wire bytes, not arbitrary enum ordinals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    Handshake = 0,
    Audio = 1,
    Text = 2,
    Control = 3,
    MetaData = 4,
    Error = 5,
    Ping = 6,
}

impl MsgType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Handshake),
            1 => Some(Self::Audio),
            2 => Some(Self::Text),
            3 => Some(Self::Control),
            4 => Some(Self::MetaData),
            5 => Some(Self::Error),
            6 => Some(Self::Ping),
            _ => None,
        }
    }
}

/// Control sub-byte ([`MsgType::Control`] payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Control {
    Start = 0,
    EndTurn = 1,
    Pause = 2,
    Restart = 3,
}

/// Events the background receive loop emits for the main loop to consume.
#[derive(Debug, Clone)]
pub enum MoshiEvent {
    /// Assistant text delta — concatenate to render the transcript.
    Text(String),
    /// Decoded assistant audio PCM (24 kHz mono f32). Only with `moshi-opus`.
    Audio(Vec<f32>),
    /// Server-reported error string (`0x05` frame).
    Error(String),
    /// WebSocket closed (graceful or error). `loaded` becomes false.
    Disconnected(String),
}

/// Commands enqueued by sync trait methods for the background send task.
enum SendCommand {
    /// 16 kHz mono f32 mic samples to encode + send.
    Pcm(Vec<f32>),
    /// Send a control frame (`0x03 + byte`).
    Control(Control),
    /// Stop the send task and close the WebSocket.
    Shutdown,
}

/// Resolved Moshi backend configuration (from [`crate::config::VoiceFrontendConfig`]).
#[derive(Debug, Clone)]
pub struct MoshiConfig {
    pub host: String,
    pub port: u16,
    pub model_repo: String,
    pub device: String,
    pub bin: PathBuf,
}

impl MoshiConfig {
    /// WebSocket URL: `wss://<host>:<port>/api/chat`.
    pub fn ws_url(&self) -> String {
        format!("wss://{}:{}/api/chat", self.host, self.port)
    }
}

/// Audio codec abstraction. The real Opus/OGG impl ([`OpusOggCodec`]) lives
/// behind `moshi-opus`; a stub is used otherwise so the base `moshi` feature
/// compiles without libopus.
trait MoshiAudioCodec: Send {
    /// OGG bytes to send before the first Opus data frame (OpusHead +
    /// OpusTags header pages). Returns one binary-frame payload per page
    /// (each will be sent as `0x01 + bytes`).
    fn header_pages(&mut self) -> Result<Vec<Vec<u8>>>;
    /// Encode a chunk of 24 kHz mono f32 PCM into zero or more OGG-page
    /// binary-frame payloads. Returns empty until a full 960-sample Opus
    /// frame has accumulated.
    fn encode_pcm(&mut self, pcm_24k: &[f32]) -> Result<Vec<Vec<u8>>>;
    /// Decode an incoming `0x01` audio-frame payload (OGG bytes) into 24 kHz
    /// mono f32 PCM. Stateful (keeps an OGG reader + Opus decoder).
    fn decode_audio(&mut self, ogg_bytes: &[u8]) -> Result<Vec<f32>>;
}

/// Stub codec used when `moshi-opus` is off. Audio I/O returns a clear error
/// instead of silently dropping, so the dashboard / repair agent can tell
/// the user to enable the feature + install libopus.
#[cfg(not(feature = "moshi-opus"))]
struct NoAudioCodec;

#[cfg(not(feature = "moshi-opus"))]
impl MoshiAudioCodec for NoAudioCodec {
    fn header_pages(&mut self) -> Result<Vec<Vec<u8>>> {
        bail_codec()
    }
    fn encode_pcm(&mut self, _pcm_24k: &[f32]) -> Result<Vec<Vec<u8>>> {
        bail_codec()
    }
    fn decode_audio(&mut self, _ogg: &[u8]) -> Result<Vec<f32>> {
        bail_codec()
    }
}

#[cfg(feature = "moshi-opus")]
mod opus_codec {
    use std::collections::VecDeque;

    use super::MoshiAudioCodec;
    use anyhow::{Context, Result};

    /// OGG serial number for the Moshi audio stream (the reference client
    /// uses 42; the server's OGG reader accepts any single serial).
    const OGG_SERIAL: u32 = 42;
    /// Opus frame size in samples @ 24 kHz (40 ms — the Moshi protocol's
    /// fixed frame length).
    const OPUS_FRAME: usize = 960;

    /// Opus/OGG codec translating the reference client
    /// (`rust/moshi-cli/src/multistream.rs`) + backend header layout
    /// (`rust/moshi-backend/src/audio.rs`). Verified against `main`.
    ///
    /// **Send side** accumulates 24 kHz mono f32, Opus-encodes 960-sample
    /// frames (Voip application), and wraps each in an OGG `EndPage`
    /// (serial 42). `header_pages()` emits OpusHead + OpusTags as two
    /// separate `EndPage`s so the server's OGG reader is primed.
    ///
    /// **Recv side** is seek-free streaming OGG demux: incoming `0x01`
    /// frame bytes accumulate in `in_buf`; `PageParser` splits them into
    /// `OggPage`s; `BasePacketReader` reassembles Opus packets (a packet
    /// may span pages); OpusHead/Tags are skipped; `opus::Decoder` decodes
    /// to 24 kHz mono f32.
    pub struct OpusOggCodec<'a> {
        // --- send/encode ---
        encoder: opus::Encoder,
        pw: ogg::PacketWriter<'a, Vec<u8>>,
        out_pcm: VecDeque<f32>,
        out_pcm_buf: Vec<u8>,
        total_data: u64,
        // --- recv/decode ---
        decoder: opus::Decoder,
        base: ogg::reading::BasePacketReader,
        in_buf: Vec<u8>,
        decode_state: DecodeState,
        pcm_buf: Vec<f32>,
    }

    /// Streaming OGG page-splitter state. A page is consumed in three
    /// stages: 27-byte header → segment table → packet data.
    enum DecodeState {
        Header,
        Segments {
            parser: ogg::reading::PageParser,
            seg_len: usize,
        },
        PacketData {
            parser: ogg::reading::PageParser,
            pd_len: usize,
        },
    }

    impl<'a> OpusOggCodec<'a> {
        pub fn new() -> Result<Self> {
            let encoder = opus::Encoder::new(24_000, opus::Channels::Mono, opus::Application::Voip)
                .context("opus encoder")?;
            let decoder =
                opus::Decoder::new(24_000, opus::Channels::Mono).context("opus decoder")?;
            Ok(Self {
                encoder,
                pw: ogg::PacketWriter::new(Vec::new()),
                out_pcm: VecDeque::new(),
                out_pcm_buf: vec![0u8; 50_000],
                total_data: 0,
                decoder,
                base: ogg::reading::BasePacketReader::new(),
                in_buf: Vec::new(),
                decode_state: DecodeState::Header,
                pcm_buf: vec![0f32; 24_000 * 2],
            })
        }
    }

    /// OpusHead (OggOpus ID header) — exact byte layout from
    /// `moshi-backend/src/audio.rs::write_opus_header`.
    fn opus_header_bytes() -> Vec<u8> {
        let mut w = Vec::with_capacity(19);
        w.extend_from_slice(b"OpusHead");
        w.push(1); // version
        w.push(1); // channel count (mono)
        w.extend_from_slice(&3840u16.to_le_bytes()); // pre-skip
        w.extend_from_slice(&48_000u32.to_le_bytes()); // sample-rate (Hz)
        w.extend_from_slice(&0i16.to_le_bytes()); // output gain (Q7.8 dB)
        w.push(0); // channel mapping family
        w
    }

    /// OpusTags (comment header) — vendor "KyutaiMoshi", 0 tags.
    fn opus_tags_bytes() -> Vec<u8> {
        let vendor = b"KyutaiMoshi";
        let mut w = Vec::with_capacity(8 + 4 + vendor.len() + 4);
        w.extend_from_slice(b"OpusTags");
        w.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        w.extend_from_slice(vendor);
        w.extend_from_slice(&0u32.to_le_bytes()); // tag count
        w
    }

    /// Emit one OGG `EndPage` containing `packet` (serial 42, granule 0)
    /// and drain the writer, returning the page bytes.
    fn one_page(
        pw: &mut ogg::PacketWriter<Vec<u8>>,
        packet: Vec<u8>,
        absgp: u64,
    ) -> Result<Vec<u8>> {
        pw.write_packet(packet, OGG_SERIAL, ogg::PacketWriteEndInfo::EndPage, absgp)
            .context("ogg write_packet")?;
        Ok(std::mem::take(pw.inner_mut()))
    }

    impl<'a> MoshiAudioCodec for OpusOggCodec<'a> {
        fn header_pages(&mut self) -> Result<Vec<Vec<u8>>> {
            // Each header is its own EndPage so the server's OGG reader sees
            // a complete stream start before any audio.
            let head = one_page(&mut self.pw, opus_header_bytes(), 0)?;
            let tags = one_page(&mut self.pw, opus_tags_bytes(), 0)?;
            Ok(vec![head, tags])
        }

        fn encode_pcm(&mut self, pcm_24k: &[f32]) -> Result<Vec<Vec<u8>>> {
            self.out_pcm.extend(pcm_24k);
            self.total_data += pcm_24k.len() as u64;
            let nchunks = self.out_pcm.len() / OPUS_FRAME;
            let mut pages = Vec::new();
            for _ in 0..nchunks {
                let mut chunk = Vec::with_capacity(OPUS_FRAME);
                for _ in 0..OPUS_FRAME {
                    let v = self
                        .out_pcm
                        .pop_front()
                        .context("opus encode pcm underflow")?;
                    chunk.push(v);
                }
                let size = self
                    .encoder
                    .encode_float(&chunk, &mut self.out_pcm_buf)
                    .context("opus encode_float")?;
                if size > 0 {
                    let page = one_page(
                        &mut self.pw,
                        self.out_pcm_buf[..size].to_vec(),
                        self.total_data,
                    )?;
                    if !page.is_empty() {
                        pages.push(page);
                    }
                }
            }
            Ok(pages)
        }

        fn decode_audio(&mut self, ogg_bytes: &[u8]) -> Result<Vec<f32>> {
            self.in_buf.extend_from_slice(ogg_bytes);
            let mut out = Vec::new();
            loop {
                match self.try_parse_page()? {
                    Some(page) => {
                        self.base.push_page(page).context("ogg push_page")?;
                    }
                    None => break,
                }
                // Drain any complete packets the page(s) produced.
                while let Some(pkt) = self.base.read_packet() {
                    if pkt.data.starts_with(b"OpusHead") || pkt.data.starts_with(b"OpusTags") {
                        continue;
                    }
                    let n = self
                        .decoder
                        .decode_float(&pkt.data, &mut self.pcm_buf, false)
                        .context("opus decode_float")?;
                    if n > 0 {
                        out.extend_from_slice(&self.pcm_buf[..n]);
                    }
                }
            }
            Ok(out)
        }
    }

    impl<'a> OpusOggCodec<'a> {
        /// Try to parse one complete OGG page from `in_buf`. Returns
        /// `Ok(None)` when not enough bytes are buffered yet (the partial
        /// page stays in `in_buf` for the next call). Resyncs on a missing
        /// `OggS` capture pattern by dropping bytes until the next one.
        fn try_parse_page(&mut self) -> Result<Option<ogg::reading::OggPage>> {
            loop {
                let state = std::mem::replace(&mut self.decode_state, DecodeState::Header);
                match state {
                    DecodeState::Header => {
                        // Resync on capture pattern "OggS".
                        while self.in_buf.len() >= 4 && &self.in_buf[0..4] != b"OggS" {
                            self.in_buf.remove(0);
                        }
                        if self.in_buf.len() < 27 {
                            self.decode_state = DecodeState::Header;
                            return Ok(None);
                        }
                        let header: [u8; 27] =
                            self.in_buf[..27].try_into().expect("checked len >= 27");
                        let (parser, seg_len) =
                            ogg::reading::PageParser::new(header).context("ogg page header")?;
                        self.in_buf.drain(..27);
                        self.decode_state = DecodeState::Segments { parser, seg_len };
                    }
                    DecodeState::Segments {
                        mut parser,
                        seg_len,
                    } => {
                        if self.in_buf.len() < seg_len {
                            self.decode_state = DecodeState::Segments { parser, seg_len };
                            return Ok(None);
                        }
                        let seg_buf = self.in_buf[..seg_len].to_vec();
                        self.in_buf.drain(..seg_len);
                        let pd_len = parser.parse_segments(seg_buf);
                        self.decode_state = DecodeState::PacketData { parser, pd_len };
                    }
                    DecodeState::PacketData { parser, pd_len } => {
                        if self.in_buf.len() < pd_len {
                            self.decode_state = DecodeState::PacketData { parser, pd_len };
                            return Ok(None);
                        }
                        let pd = self.in_buf[..pd_len].to_vec();
                        self.in_buf.drain(..pd_len);
                        let page = parser
                            .parse_packet_data(pd)
                            .context("ogg parse_packet_data")?;
                        self.decode_state = DecodeState::Header;
                        return Ok(Some(page));
                    }
                }
            }
        }
    }
}

#[cfg(feature = "moshi-opus")]
pub use opus_codec::OpusOggCodec;

#[cfg(feature = "moshi-opus")]
fn make_codec() -> Result<Box<dyn MoshiAudioCodec>> {
    Ok(Box::new(OpusOggCodec::new()?))
}

#[cfg(not(feature = "moshi-opus"))]
fn make_codec() -> Result<Box<dyn MoshiAudioCodec>> {
    Ok(Box::new(NoAudioCodec))
}

#[cfg(not(feature = "moshi-opus"))]
fn bail_codec<T>() -> Result<T> {
    anyhow::bail!(
        "Moshi audio I/O requires the `moshi-opus` cargo feature + libopus at \
         build time (see voice/moshi.rs docs). Build with --features moshi-opus \
         and install libopus (vcpkg `opus`, or the `opus` crate's vendored build)."
    )
}

/// Shared state between the [`MoshiFrontend`] handle and its background tasks.
struct MoshiShared {
    active: AtomicBool,
    loaded: AtomicBool,
    /// When true, the receive loop discards assistant audio (barge-in /
    /// orchestrator-speaking). Set by `interrupt()`, cleared by `resume()`.
    muted: AtomicBool,
    /// Sender the sync trait methods use to enqueue send-task commands.
    send_tx: Mutex<Option<UnboundedSender<SendCommand>>>,
    /// Sender the receive task uses to emit [`MoshiEvent`]s to the main loop.
    /// Held in a Mutex so the frontend can replace it after a reconnect.
    event_tx: Mutex<Option<UnboundedSender<MoshiEvent>>>,
    child: Mutex<Option<std::process::Child>>,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Kyutai Moshi S2S front-end.
///
/// Construct with [`MoshiFrontend::new`]; drive via the
/// [`VoiceFrontend`] trait. Assistant text/audio/error events are consumed
/// through [`MoshiFrontend::events`].
pub struct MoshiFrontend {
    cfg: MoshiConfig,
    shared: std::sync::Arc<MoshiShared>,
    /// Event receiver, handed out exactly once via [`Self::events`].
    event_rx: Mutex<Option<UnboundedReceiver<MoshiEvent>>>,
}

impl std::fmt::Debug for MoshiFrontend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoshiFrontend")
            .field("cfg", &self.cfg)
            .field("active", &self.shared.active.load(Ordering::Relaxed))
            .field("loaded", &self.shared.loaded.load(Ordering::Relaxed))
            .finish()
    }
}

impl MoshiFrontend {
    /// Build a frontend from resolved config. Cheap; does not start anything.
    /// Call [`VoiceFrontend::start`] to spawn the subprocess + connect.
    pub fn new(cfg: MoshiConfig) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<MoshiEvent>();
        let shared = std::sync::Arc::new(MoshiShared {
            active: AtomicBool::new(false),
            loaded: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            send_tx: Mutex::new(None),
            event_tx: Mutex::new(Some(event_tx)),
            child: Mutex::new(None),
            join: Mutex::new(None),
        });
        Self {
            cfg,
            shared,
            event_rx: Mutex::new(Some(event_rx)),
        }
    }

    /// Take the event receiver, once. Returns `None` on subsequent calls.
    /// The main loop calls this at boot to receive assistant text/audio/errors.
    pub fn events(&self) -> Option<UnboundedReceiver<MoshiEvent>> {
        self.event_rx.lock().ok()?.take()
    }

    /// Whether the backend subprocess is alive and the WebSocket is connected.
    pub fn loaded(&self) -> bool {
        self.shared.loaded.load(Ordering::Relaxed)
    }

    /// Clear the barge-in mute so the receive loop forwards assistant audio
    /// again. Concrete method (not on the trait); called by the Phase 3
    /// bridge after an orchestrator turn ends.
    pub fn resume(&self) {
        self.shared.muted.store(false, Ordering::Relaxed);
    }

    /// Whether output is currently muted (barge-in / orchestrator speaking).
    pub fn muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    /// Spawn the `moshi-backend` subprocess with a generated config.json.
    fn spawn_backend(&self) -> Result<std::process::Child> {
        let bin = if self.cfg.bin.as_os_str().is_empty() {
            resolve_moshi_binary()?
        } else {
            self.cfg.bin.clone()
        };
        if !bin.exists() {
            anyhow::bail!(
                "moshi-backend binary not found at {bin:?}; build it (CUDA) or set \
                 CONTINUUM_MOSHI_BIN / voice.frontend.moshi_bin"
            );
        }
        let config_path = write_moshi_config_json(&self.cfg)?;
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("--config")
            .arg(&config_path)
            .arg("standalone")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn moshi-backend at {bin:?}"))?;
        info!(layer = "voice", component = TAG, bin = ?bin, "moshi-backend subprocess spawned");
        Ok(child)
    }
}

impl VoiceFrontend for MoshiFrontend {
    fn mode(&self) -> &'static str {
        "moshi"
    }

    fn start(&self) -> Result<()> {
        if self.shared.active.load(Ordering::Relaxed) {
            return Ok(()); // idempotent
        }
        let child = self.spawn_backend()?;
        *self.shared.child.lock().unwrap() = Some(child);

        // Grab the tokio handle the background tasks will run on. Requires a
        // running multi-thread runtime (the continuum main loop).
        let handle = tokio::runtime::Handle::try_current()
            .context("MoshiFrontend::start must be called from a tokio runtime context")?;

        let (send_tx, send_rx) = mpsc::unbounded_channel::<SendCommand>();
        *self.shared.send_tx.lock().unwrap() = Some(send_tx);
        let shared = self.shared.clone();
        let cfg = self.cfg.clone();
        let event_tx = self
            .shared
            .event_tx
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("moshi event sender already taken"))?;

        self.shared.active.store(true, Ordering::Relaxed);
        let join = handle.spawn(moshi_session_task(cfg, send_rx, event_tx, shared));
        *self.shared.join.lock().unwrap() = Some(join);
        Ok(())
    }

    fn stop(&self) {
        self.shared.active.store(false, Ordering::Relaxed);
        self.shared.loaded.store(false, Ordering::Relaxed);
        // Signal the send task to close the WebSocket and exit.
        if let Some(tx) = self.shared.send_tx.lock().unwrap().take() {
            let _ = tx.send(SendCommand::Shutdown);
        }
        // Kill the subprocess.
        if let Some(mut child) = self.shared.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Detach the join handle; the task exits on its own.
        if let Some(join) = self.shared.join.lock().unwrap().take() {
            join.abort();
        }
        info!(layer = "voice", component = TAG, "moshi frontend stopped");
    }

    fn is_active(&self) -> bool {
        self.shared.active.load(Ordering::Relaxed)
    }

    fn loaded(&self) -> bool {
        self.loaded()
    }

    fn interrupt(&self) {
        // Barge-in / orchestrator-speaking: mute assistant audio output and
        // send an EndTurn hint. The model self-interrupts on incoming audio;
        // muting stops us from playing stale Moshi audio over the orchestrator.
        self.shared.muted.store(true, Ordering::Relaxed);
        if let Some(tx) = self.shared.send_tx.lock().unwrap().as_ref() {
            let _ = tx.send(SendCommand::Control(Control::EndTurn));
        }
        debug!(
            layer = "voice",
            component = TAG,
            "moshi interrupted (muted + EndTurn)"
        );
    }

    fn feed_pcm(&self, samples: &[f32]) {
        if !self.is_active() {
            return;
        }
        if let Some(tx) = self.shared.send_tx.lock().unwrap().as_ref() {
            // Cheap: unbounded channel; the send task does the heavy lifting.
            let _ = tx.send(SendCommand::Pcm(samples.to_vec()));
        }
    }
}

/// Background task: connect WSS, run the send + receive loops.
async fn moshi_session_task(
    cfg: MoshiConfig,
    mut send_rx: UnboundedReceiver<SendCommand>,
    event_tx: UnboundedSender<MoshiEvent>,
    shared: std::sync::Arc<MoshiShared>,
) {
    if let Err(e) = run_session(&cfg, &mut send_rx, &event_tx, &shared).await {
        warn!(layer = "voice", component = TAG, error = %e, "moshi session ended with error");
        let _ = event_tx.send(MoshiEvent::Disconnected(format!("{e:#}")));
    }
    shared.loaded.store(false, Ordering::Relaxed);
    shared.active.store(false, Ordering::Relaxed);
}

async fn run_session(
    cfg: &MoshiConfig,
    send_rx: &mut UnboundedReceiver<SendCommand>,
    event_tx: &UnboundedSender<MoshiEvent>,
    shared: &std::sync::Arc<MoshiShared>,
) -> Result<()> {
    // Wait for the backend to start listening. Poll-connect with backoff.
    let url = cfg.ws_url();
    let ws = connect_with_backoff(&url).await?;
    shared.loaded.store(true, Ordering::Relaxed);
    info!(layer = "voice", component = TAG, url = %url, "moshi websocket connected");

    use futures::SinkExt;
    use futures::StreamExt; // split()
    use tokio_tungstenite::tungstenite::protocol::Message; // send()
    let (mut ws_sink, mut ws_stream) = ws.split();

    // First, drain the server's opening messages (handshake + metadata).
    // They arrive before any audio/text. We read until we've seen the
    // handshake; subsequent messages are handled in the main receive loop.
    drain_handshake(&mut ws_stream).await?;

    // Send the OpusHead/OpusTags header pages as the first audio frames, so
    // the server's OGG reader is primed before we send any Opus data.
    // `enc_codec` stays with the send task; `dec_codec` (a fresh codec so
    // its OGG demuxer + Opus decoder start clean) moves to the receive task.
    let mut enc_codec = make_codec()?;
    let dec_codec = make_codec()?;
    match enc_codec.header_pages() {
        Ok(pages) => {
            for page in pages {
                let mut frame = Vec::with_capacity(page.len() + 1);
                frame.push(MsgType::Audio as u8);
                frame.extend_from_slice(&page);
                ws_sink.send(Message::Binary(frame)).await?;
            }
        }
        Err(e) => {
            // No libopus / codec unimplemented: audio won't flow, but text will.
            warn!(layer = "voice", component = TAG, error = %e, "moshi audio codec unavailable; text-only");
        }
    }

    // Resample 16 kHz → 24 kHz for the encoder. We do a naive linear/hold
    // resample here only behind moshi-opus; without it we never have audio to
    // send anyway. rubato is the crate continuum already uses elsewhere, but
    // pulling it into this async task would need it as a dep here — instead
    // we feed the codec raw and let the (unimplemented) encoder handle frame
    // accumulation. For the stub this path is inert.
    let mut pcm_buf_16k: Vec<f32> = Vec::new();

    // Send loop: drain send_rx, encode, send. Receive loop: read WS frames.
    // Run both concurrently.
    let send_task = {
        let mut ws_sink = ws_sink;
        async move {
            let mut codec = enc_codec;
            while let Some(cmd) = send_rx.recv().await {
                match cmd {
                    SendCommand::Pcm(samples) => {
                        pcm_buf_16k.extend_from_slice(&samples);
                        // Resample 16k→24k (ratio 1.5). Behind moshi-opus the
                        // codec accumulates 960-sample frames; here we just
                        // hand the 24k-equivalent off. Naive linear resample:
                        let pcm_24k = resample_16k_to_24k(&pcm_buf_16k);
                        pcm_buf_16k.clear();
                        match codec.encode_pcm(&pcm_24k) {
                            Ok(frames) => {
                                for f in frames {
                                    let mut out = Vec::with_capacity(f.len() + 1);
                                    out.push(MsgType::Audio as u8);
                                    out.extend_from_slice(&f);
                                    if ws_sink.send(Message::Binary(out)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(_) => {
                                // Codec unavailable; samples dropped (logged once below).
                                static ONCE: std::sync::Once = std::sync::Once::new();
                                ONCE.call_once(|| {
                                    warn!(layer="voice", component=TAG, "moshi feed_pcm: audio codec unavailable, dropping mic samples");
                                });
                            }
                        }
                    }
                    SendCommand::Control(c) => {
                        let msg = vec![MsgType::Control as u8, c as u8];
                        let _ = ws_sink.send(Message::Binary(msg)).await;
                    }
                    SendCommand::Shutdown => {
                        let _ = ws_sink.send(Message::Close(None)).await;
                        return;
                    }
                }
            }
        }
    };

    let recv_task = async move {
        // Persistent decode codec — its OGG demuxer + Opus decoder must
        // survive across frames (a packet can span OGG pages, and the
        // decoder holds state). Creating one per frame would reset both.
        let mut dec = dec_codec;
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Binary(bin)) if !bin.is_empty() => {
                    let mt = match MsgType::from_u8(bin[0]) {
                        Some(t) => t,
                        None => {
                            debug!(
                                layer = "voice",
                                component = TAG,
                                "unknown moshi msg byte {}",
                                bin[0]
                            );
                            continue;
                        }
                    };
                    match mt {
                        MsgType::Handshake | MsgType::MetaData => {} // already drained / ignore
                        MsgType::Text => {
                            if let Ok(s) = std::str::from_utf8(&bin[1..]) {
                                let _ = event_tx.send(MoshiEvent::Text(s.to_string()));
                            }
                        }
                        MsgType::Audio => {
                            if shared.muted.load(Ordering::Relaxed) {
                                continue; // barge-in / orchestrator speaking
                            }
                            match dec.decode_audio(&bin[1..]) {
                                Ok(pcm) if !pcm.is_empty() => {
                                    let _ = event_tx.send(MoshiEvent::Audio(pcm));
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    debug!(
                                        layer = "voice",
                                        component = TAG,
                                        error = %e,
                                        "moshi audio decode error; dropping frame"
                                    );
                                }
                            }
                        }
                        MsgType::Error => {
                            let s = std::str::from_utf8(&bin[1..])
                                .unwrap_or("<non-utf8 error>")
                                .to_string();
                            warn!(layer = "voice", component = TAG, "moshi server error: {s}");
                            let _ = event_tx.send(MoshiEvent::Error(s));
                        }
                        MsgType::Control | MsgType::Ping => {}
                    }
                }
                Ok(Message::Close(_)) | Err(_) => {
                    let _ = event_tx.send(MoshiEvent::Disconnected("websocket closed".into()));
                    break;
                }
                _ => {}
            }
        }
    };

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
    Ok(())
}

/// Read and discard server-opening Handshake + MetaData frames.
async fn drain_handshake<S>(ws: &mut S) -> Result<()>
where
    S: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::protocol::Message; // ws.next()
    let mut got_handshake = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !got_handshake {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .context("timeout waiting for moshi handshake")?
            .ok_or_else(|| anyhow::anyhow!("stream closed before handshake"))?;
        match msg {
            Ok(Message::Binary(bin)) if !bin.is_empty() => {
                match MsgType::from_u8(bin[0]) {
                    Some(MsgType::Handshake) => got_handshake = true,
                    Some(MsgType::MetaData) => {
                        debug!(
                            layer = "voice",
                            component = TAG,
                            "moshi metadata frame discarded"
                        );
                    }
                    _ => {
                        // Some other frame before handshake — keep it; rare.
                        debug!(
                            layer = "voice",
                            component = TAG,
                            "early moshi frame type {}",
                            bin[0]
                        );
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => {
                anyhow::bail!("moshi websocket closed before handshake");
            }
            _ => {}
        }
    }
    Ok(())
}

/// Connect to the Moshi WebSocket, accepting the self-signed cert. Polls
/// for up to ~10 s so the backend has time to bind after we spawn it.
async fn connect_with_backoff(
    url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    use tokio_tungstenite::{connect_async_tls_with_config, Connector};

    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("build native-tls connector")?;
    let connector = Connector::NativeTls(connector);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut delay = Duration::from_millis(200);
    loop {
        match connect_async_tls_with_config(url, None, false, Some(connector.clone())).await {
            Ok((ws, _resp)) => return Ok(ws),
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(e).context("moshi websocket connect timed out");
                }
                debug!(layer="voice", component=TAG, error=%e, "moshi connect retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(1000));
            }
        }
    }
}

/// Naive 16 kHz → 24 kHz linear interpolation (ratio 1.5). Sufficient for the
/// stub path; the real implementation should use `rubato` (already a continuum
/// dep behind `runtime`) for quality, but this keeps moshi.rs self-contained.
fn resample_16k_to_24k(input: &[f32]) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let out_len = (input.len() as f32 * 1.5).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f32 / 1.5;
        let i0 = src.floor() as usize;
        let i1 = (i0 + 1).min(input.len() - 1);
        let frac = src - i0 as f32;
        out.push(input[i0] * (1.0 - frac) + input[i1] * frac);
    }
    out
}

/// Resolve the `moshi-backend` executable:
/// `CONTINUUM_MOSHI_BIN` env → `~/.continuum-dev/bin/moshi/moshi-backend.exe`
/// (Windows) / `moshi-backend` (unix) → `moshi-backend` on PATH.
pub fn resolve_moshi_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("CONTINUUM_MOSHI_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
    }
    let dev = continuum_dev_bin_dir()?;
    let exe = if cfg!(windows) {
        "moshi-backend.exe"
    } else {
        "moshi-backend"
    };
    let candidate = dev.join("moshi").join(exe);
    if candidate.exists() {
        return Ok(candidate);
    }
    // Last resort: assume it's on PATH.
    Ok(PathBuf::from("moshi-backend"))
}

fn continuum_dev_bin_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".continuum-dev").join("bin"))
}

/// Write a `moshi-backend` config.json for this session and return its path.
///
/// Model files are expected under
/// `~/.continuum-dev/models/moshi/<repo-basename>/` (placed by
/// `scripts/download-models.ps1`). The server auto-downloads any missing
/// file from `hf_repo`, so exact filenames need not be perfect.
fn write_moshi_config_json(cfg: &MoshiConfig) -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    let dev = home.join(".continuum-dev");
    let model_dir = dev
        .join("models")
        .join("moshi")
        .join(repo_basename(&cfg.model_repo));
    let log_dir = dev.join("logs").join("moshi");
    let cert_dir = dev.join("moshi-certs");
    std::fs::create_dir_all(&model_dir)?;
    std::fs::create_dir_all(&log_dir)?;
    std::fs::create_dir_all(&cert_dir)?;

    let config = serde_json::json!({
        "instance_name": "continuum",
        "hf_repo": cfg.model_repo,
        "lm_model_file": model_dir.join("model.q8.gguf").to_string_lossy(),
        "text_tokenizer_file": model_dir.join("tokenizer_spm_32k_3.model").to_string_lossy(),
        "mimi_model_file": model_dir.join("tokenizer-e351c8d8-checkpoint125.safetensors").to_string_lossy(),
        "mimi_num_codebooks": 8,
        "log_dir": log_dir.to_string_lossy(),
        "static_dir": dev.join("moshi-static").to_string_lossy(),
        "addr": cfg.host,
        "port": cfg.port,
        "cert_dir": cert_dir.to_string_lossy(),
        "use_cpu_for_mimi": cfg.device != "cuda",
    });
    let path = dev.join("moshi-backend-config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    debug!(
        layer = "voice",
        component = TAG,
        ?path,
        "wrote moshi-backend config"
    );
    Ok(path)
}

fn repo_basename(repo: &str) -> String {
    repo.rsplit('/').next().unwrap_or(repo).to_string()
}

impl Drop for MoshiFrontend {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_type_round_trips() {
        for b in 0..=6u8 {
            if let Some(mt) = MsgType::from_u8(b) {
                assert_eq!(mt as u8, b);
            }
        }
        assert!(MsgType::from_u8(7).is_none());
    }

    #[test]
    fn ws_url_format() {
        let cfg = MoshiConfig {
            host: "127.0.0.1".into(),
            port: 8084,
            model_repo: "kyutai/moshiko-candle-q8".into(),
            device: "cuda".into(),
            bin: PathBuf::new(),
        };
        assert_eq!(cfg.ws_url(), "wss://127.0.0.1:8084/api/chat");
    }

    #[test]
    fn resample_16k_to_24k_doubles_length_ratio() {
        let input = vec![0.0, 1.0, 0.0, -1.0]; // 4 samples @ 16k
        let out = resample_16k_to_24k(&input);
        assert_eq!(out.len(), 6); // 4 * 1.5 = 6
                                  // Integer-src indices preserve the source exactly: out[0] (src=0)
                                  // and out[3] (src=2). Interpolated indices do not.
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[3] - 0.0).abs() < 1e-6);
        // out[1] (src=0.667) interpolates input[0]=0.0 and input[1]=1.0.
        assert!((out[1] - 0.0).abs() > 0.1);
    }

    #[test]
    fn resample_empty_input() {
        assert!(resample_16k_to_24k(&[]).is_empty());
    }

    #[test]
    fn repo_basename_strips_org() {
        assert_eq!(
            repo_basename("kyutai/moshiko-candle-q8"),
            "moshiko-candle-q8"
        );
        assert_eq!(repo_basename("plain"), "plain");
    }

    #[cfg(not(feature = "moshi-opus"))]
    #[test]
    fn no_audio_codec_bails_clearly() {
        let mut c = NoAudioCodec;
        assert!(c.header_pages().is_err());
        assert!(c.encode_pcm(&[0.0; 960]).is_err());
        assert!(c.decode_audio(&[]).is_err());
    }

    /// End-to-end proof the Opus/OGG codec is real, not a stub: encode 3
    /// 960-sample frames of a 220 Hz sine, wrap each in an OGG EndPage, feed
    /// the pages (headers first, then audio) back through the decoder, and
    /// confirm we get 2880 samples of non-silent audio back out. Gated behind
    /// `moshi-opus` (needs libopus at build + runtime).
    #[cfg(feature = "moshi-opus")]
    #[test]
    fn opus_codec_roundtrips_audio() {
        let mut enc = opus_codec::OpusOggCodec::new().expect("codec new");
        let mut dec = opus_codec::OpusOggCodec::new().expect("codec new");

        // Header pages prime the decoder and must yield no PCM.
        let heads = enc.header_pages().expect("header_pages");
        assert_eq!(heads.len(), 2, "OpusHead + OpusTags");
        for page in &heads {
            assert!(
                dec.decode_audio(page).expect("decode header").is_empty(),
                "header pages produce no PCM"
            );
        }

        // 3 frames @ 960 samples = 2880 samples of a 220 Hz sine @ 24 kHz.
        let n_frames = 3;
        let total = n_frames * 960; // OPUS_FRAME @ 24 kHz (40 ms)
        let pcm: Vec<f32> = (0..total)
            .map(|i| (2.0 * std::f32::consts::PI * 220.0 * (i as f32) / 24_000.0).sin() * 0.5)
            .collect();
        let pages = enc.encode_pcm(&pcm).expect("encode_pcm");
        assert_eq!(pages.len(), n_frames, "one EndPage per 960-sample frame");

        let mut dec_pcm = Vec::new();
        for page in &pages {
            dec_pcm.extend(dec.decode_audio(page).expect("decode audio"));
        }
        assert_eq!(dec_pcm.len(), total, "decoded sample count matches input");
        // Opus is lossy, but a 220 Hz sine at amplitude 0.5 must come back
        // with real energy — not silence, not a stub no-op.
        let energy: f32 = dec_pcm.iter().map(|s| s * s).sum();
        assert!(energy > 1.0, "decoded audio has energy, not silence");
    }

    #[test]
    fn frontend_events_handed_out_once() {
        let cfg = MoshiConfig {
            host: "127.0.0.1".into(),
            port: 8084,
            model_repo: "kyutai/moshiko-candle-q8".into(),
            device: "cuda".into(),
            bin: PathBuf::new(),
        };
        let fe = MoshiFrontend::new(cfg);
        assert!(fe.events().is_some());
        assert!(fe.events().is_none()); // exactly once
        assert_eq!(fe.mode(), "moshi");
        assert!(!fe.is_active());
        assert!(!fe.loaded());
        fe.interrupt();
        assert!(fe.muted());
        fe.resume();
        assert!(!fe.muted());
    }
}
