//! # Privacy filter — the choke point for every observed byte
//!
//! This module is the single privacy boundary of the context engine (spec
//! §4.1). Every byte a collector observes passes through here **at collector
//! emit** — before the live-context hub, before persistence, before any model
//! (local models included), and before any MCP tool response. It is
//! deliberately **not** gated behind the `runtime` feature: the runtime, the
//! MCP server, and the desktop app all link it, so it must compile under
//! `--no-default-features`.
//!
//! ## The free-text vs. structured-field contract (READ THIS)
//!
//! The secret/entropy scrubbers ([`PrivacyFilter::scrub_text`]) apply to
//! **free-text fields only**: window titles, vision captions, audio
//! transcripts, summaries, commit *subjects*. In free text, anything that
//! looks like a secret — including a 40-char git object id, a UUID, or a
//! sha256 digest — **is redacted on purpose**: inside prose we cannot tell a
//! commit hash from a leaked token, so we err on the side of redaction.
//!
//! **Structured fields from trusted collectors are exempt *by
//! construction*** — the exemption is not a smarter regex, it is that
//! collectors simply never call [`PrivacyFilter::scrub_text`] on structured
//! values (git commit ids, branch names, frame ids, dedupe keys). A git
//! collector scrubs the commit *subject* (free text) and passes the commit
//! *id* (structured) through untouched. [`PrivacyFilter::scrub_path`] and
//! [`PrivacyFilter::resolve_zone`] never apply the secret scrubbers, so
//! structured values routed through them survive end-to-end.
//!
//! Callers therefore choose per field: `scrub_text` for free text,
//! passthrough (no call) for structured values, `scrub_path` for filesystem
//! paths.
//!
//! ## Zones
//!
//! Every observation resolves to a [`Zone`] via [`PrivacyFilter::resolve_zone`]:
//! `never_observe` (sentinel only, nothing recorded), `local_only` (observed
//! and persisted, stripped from everything cloud-bound), or `cloud_allowed`
//! (default). Legacy `[context]` sensitive lists are synthesised into zone
//! rules at load time ([`PrivacyFilter::from_config`]); explicit
//! `[privacy].zones` rules are unioned in and the **strictest matching zone
//! wins**. Derived artifacts inherit the strictest zone of their inputs
//! ([`strictest`]).
//!
//! Sentinel constants ([`EXCLUDED_PROCESS`], [`EXCLUDED_TITLE`],
//! [`PRIVATE_LABEL`]) define the shape of the `never_observe` sentinel
//! observation; enforcement at the collectors lands in Task A2.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

use crate::config::{ContextConfig, ObservationToggles, PrivacyConfig};
use crate::senses::live_context::PrivacyDisposition;

/// An observation source gated by the honest per-source toggles (spec §4.1).
///
/// `Window` (the foreground/context poll) has no dedicated toggle — it is
/// gated only by `pause_all`. `Files` and `Git` are enforcement seams for
/// the file watcher (Task A5) and git collector (Task A7); their toggles
/// already exist in config and are checked here so those collectors only
/// need to call [`source_enabled`] when they land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedSource {
    /// Microphone / audio observation (`toggles.mic`, enforced by AudioWatcher).
    Mic,
    /// Screen capture + vision (`toggles.screen`, enforced by VisionWatcher).
    Screen,
    /// File watcher (`toggles.files`, seam for Task A5).
    Files,
    /// Git collector (`toggles.git`, seam for Task A7).
    Git,
    /// Foreground window / context poll (gated by `pause_all` only).
    Window,
}

/// Returns whether a source is allowed to emit observations under the
/// current toggle state. `pause_all` wins over everything (spec §4.1:
/// "pause_all gates the frame loop" — each watcher additionally checks it
/// for defense in depth, so no source emits even if the frame loop gate is
/// bypassed).
pub fn source_enabled(toggles: &ObservationToggles, source: ObservedSource) -> bool {
    if toggles.pause_all {
        return false;
    }
    match source {
        ObservedSource::Mic => toggles.mic,
        ObservedSource::Screen => toggles.screen,
        ObservedSource::Files => toggles.files,
        ObservedSource::Git => toggles.git,
        ObservedSource::Window => true,
    }
}

/// Emit a privacy/toggle system event.
///
/// Since Task A6 this routes into the events channel as a real `system`
/// [`ContextEvent`] via the process-global sender
/// (`memory::events::send_system_event`) when the `runtime` feature is on
/// — the runtime installs the sender at boot with
/// `memory::events::install_global_sender`; before that (or in processes
/// without a writer, like the perception bin) the event is log-only. The
/// structured log line is always emitted.
///
/// [`ContextEvent`]: crate::memory::events::ContextEvent
pub fn emit_system_event(kind: &str, detail: &str) {
    tracing::info!(
        layer = "senses",
        component = "privacy",
        event_kind = kind,
        detail = detail,
        "privacy system event"
    );
    #[cfg(feature = "runtime")]
    crate::memory::events::send_system_event(kind, detail);
}

/// Replacement literal used by every scrubber. Chosen so that scrubbing is
/// idempotent: no scrubber pattern matches the literal itself.
pub const REDACTED: &str = "[REDACTED]";

/// Sentinel process name emitted for a `never_observe` observation (spec
/// §4.1). When the foreground window matches a `never_observe` zone rule the
/// collector emits a sentinel observation with this process name instead of
/// dropping the frame — otherwise latest-wins consumers would keep showing
/// the *previous* window as current (the stale-frame bug). Sentinel
/// semantics: no screenshot file, no caption, no events row; dwell resets;
/// switch events with an excluded endpoint are replaced by a single
/// synthetic switch from/to this bucket; salience treats sentinel↔real
/// transitions as a process change. Enforcement lands in Task A2.
pub const EXCLUDED_PROCESS: &str = "[excluded]";

/// Sentinel window title accompanying [`EXCLUDED_PROCESS`]: the empty
/// string. Titles of excluded windows are content and must not leak even in
/// redacted form.
pub const EXCLUDED_TITLE: &str = "";

/// Display label session state and dashboards use for a `never_observe`
/// span (spec §4.1: "session state shows \"[private]\"").
pub const PRIVATE_LABEL: &str = "[private]";

/// Built-in private-browsing title keywords synthesised into `local_only`
/// zone rules at load time (spec §4.1 legacy-migration defaults). These are
/// always active in addition to configured rules.
pub const PRIVATE_BROWSING_KEYWORDS: [&str; 3] = ["InPrivate", "Incognito", "Private Browsing"];

/// Observation zone for a window/process/source (spec §4.1).
///
/// Ordering of strictness: [`Zone::NeverObserve`] > [`Zone::LocalOnly`] >
/// [`Zone::CloudAllowed`]. Use [`strictest`] to combine zones — a derived
/// artifact inherits the strictest zone of its inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Zone {
    /// Nothing is recorded: the collector emits only the sentinel
    /// observation ([`EXCLUDED_PROCESS`] / [`EXCLUDED_TITLE`]).
    NeverObserve,
    /// Observed, scrubbed, persisted, local models may see it; stripped
    /// from everything cloud-bound.
    LocalOnly,
    /// Observed and eligible for cloud-bound context (still scrubbed).
    /// The default zone when no rule matches.
    #[default]
    CloudAllowed,
}

impl Zone {
    /// Numeric strictness rank used by [`strictest`]; higher is stricter.
    fn strictness(self) -> u8 {
        match self {
            Zone::NeverObserve => 2,
            Zone::LocalOnly => 1,
            Zone::CloudAllowed => 0,
        }
    }
}

/// Interop with the legacy live-context disposition enum: `never_observe`
/// maps to [`PrivacyDisposition::Excluded`] (finally produced),
/// `local_only` to [`PrivacyDisposition::Redacted`] (preserving today's
/// "observed but redacted" spirit), `cloud_allowed` to
/// [`PrivacyDisposition::Visible`].
impl From<Zone> for PrivacyDisposition {
    fn from(zone: Zone) -> Self {
        match zone {
            Zone::NeverObserve => PrivacyDisposition::Excluded,
            Zone::LocalOnly => PrivacyDisposition::Redacted,
            Zone::CloudAllowed => PrivacyDisposition::Visible,
        }
    }
}

/// Zone-propagation helper (spec §4.1): returns the strictest zone in
/// `zones`. An empty iterator yields [`Zone::CloudAllowed`] — an artifact
/// with no observed inputs has nothing to protect.
pub fn strictest<I: IntoIterator<Item = Zone>>(zones: I) -> Zone {
    zones
        .into_iter()
        .max_by_key(|z| z.strictness())
        .unwrap_or(Zone::CloudAllowed)
}

/// One zone rule from `[privacy].zones` (or synthesised from the legacy
/// `[context]` lists): `{match_process?, match_title_keyword?, zone}`.
///
/// Matching semantics: every *present* criterion must match (logical AND);
/// a rule with neither criterion never matches. `match_process` is
/// case-insensitive equality against the foreground process name;
/// `match_title_keyword` is a case-insensitive substring match against the
/// window title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneRule {
    /// Process name to match (case-insensitive equality), e.g. `"1password.exe"`.
    #[serde(default)]
    pub match_process: Option<String>,
    /// Title fragment to match (case-insensitive substring), e.g. `"Incognito"`.
    #[serde(default)]
    pub match_title_keyword: Option<String>,
    /// Zone applied when the rule matches.
    pub zone: Zone,
}

impl ZoneRule {
    /// Returns `true` when this rule matches the given foreground process
    /// name and window title. See the struct docs for semantics.
    pub fn matches(&self, process_name: &str, window_title: &str) -> bool {
        if self.match_process.is_none() && self.match_title_keyword.is_none() {
            return false;
        }
        if let Some(process) = &self.match_process {
            if !process.eq_ignore_ascii_case(process_name) {
                return false;
            }
        }
        if let Some(keyword) = &self.match_title_keyword {
            let title = window_title.to_ascii_lowercase();
            if !title.contains(&keyword.to_ascii_lowercase()) {
                return false;
            }
        }
        true
    }
}

// --- Scrubber patterns (compiled once) ---
//
// Every pattern is chosen so that the replacement literal `[REDACTED]` can
// never re-match: scrubbing is idempotent by construction.

/// `Bearer <token>` — the whole header value is replaced with
/// `Bearer [REDACTED]`.
static RE_BEARER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bBearer\s+[A-Za-z0-9._~+/-]{16,}={0,2}").expect("static bearer regex compiles")
});

/// `sk-…` API keys (OpenAI / Anthropic style). Guarded by a
/// must-contain-a-digit check in code so prose like `sk-learn-pipeline`
/// survives.
static RE_SK_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{8,}").expect("static sk regex compiles"));

/// GitHub tokens: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`.
static RE_GITHUB_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bgh[opsur]_[A-Za-z0-9]{20,}\b").expect("static github regex compiles")
});

/// GitLab personal/project/group access tokens: `glpat-…` (and the
/// sibling `gldt-`/`glrt-`/`glsoat-` families share the `gl…-` shape, so
/// the prefix alternation covers them too).
static RE_GITLAB_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bgl(?:pat|dt|rt|soat|ptt|cbt|agent|feed)-[A-Za-z0-9_-]{16,}")
        .expect("static gitlab regex compiles")
});

/// Slack tokens: `xoxb-`, `xoxp-`, `xoxs-`, `xoxa-`, `xoxr-`, `xoxe-`.
/// Slack's own segments are digit/hex runs joined by `-`, so the body
/// class deliberately includes the separator.
static RE_SLACK_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bxox[bpsare]-[A-Za-z0-9-]{10,}").expect("static slack regex compiles")
});

/// Google OAuth access tokens (`ya29.…`) and the refresh-token shape
/// (`1//0…`). The body is base64url plus `.` and `~`, which the generic
/// base64 rule cannot span because of the dot.
static RE_GOOGLE_OAUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bya29\.[A-Za-z0-9._~+/-]{20,}={0,2}").expect("static google oauth regex compiles")
});

/// JSON Web Tokens: three base64url segments joined by dots. Matched as a
/// whole so the redaction is one `[REDACTED]` rather than three, and so a
/// short (< 32 char) header/payload segment is still caught.
static RE_JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}")
        .expect("static jwt regex compiles")
});

/// AWS access key ids: `AKIA` + 16 uppercase alphanumerics.
static RE_AKIA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("static akia regex compiles"));

/// UUIDs (8-4-4-4-12 hex). Part of the entropy scrubber: in free text a
/// UUID is indistinguishable from a session token.
static RE_UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
        .expect("static uuid regex compiles")
});

/// High-entropy hex runs ≥ 32 chars (sha1/sha256 digests, git OIDs, hex
/// tokens). Deliberately unconditional in free text — see the module docs.
static RE_HEX32: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-fA-F]{32,}\b").expect("static hex regex compiles"));

/// Base64-ish runs ≥ 32 chars, in **both** the standard (`+/`) and the
/// URL-safe (`-_`) alphabet. Group 1 is the boundary character (regex has
/// no lookbehind); group 2 is the candidate token, which must additionally
/// pass a character-diversity check in code (lower + upper + digit) so long
/// plain words survive.
///
/// The URL-safe alphabet is not optional: JWT segments, `ya29.` Google
/// tokens and most modern PATs are base64url, and a standard-alphabet-only
/// class breaks such a token at its first `-`/`_` — often leaving both
/// halves under the 32-char floor, i.e. unredacted (fixwave 2, M5).
///
/// `=` **is** a valid boundary character: `export TOKEN=<payload>` is the
/// single most common shape a pasted secret arrives in, and excluding `=`
/// from the boundary class made that exact shape unmatchable. Padding at
/// the end of a preceding token is still consumed by the trailing `={0,2}`,
/// so allowing it here costs nothing.
static RE_BASE64: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|[^A-Za-z0-9+/_-])([A-Za-z0-9+/_-]{32,}={0,2})")
        .expect("static base64 regex compiles")
});

/// Card-number candidates: 13-19 digits with optional single space/dash
/// separators. Only redacted when the digits pass the Luhn check.
static RE_CARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d(?:[ -]?\d){12,18}\b").expect("static card regex compiles"));

/// IBAN candidates: 2 uppercase letters + 2 digits + 11-30 more uppercase
/// alphanumerics with optional single spaces. Only redacted when the
/// mod-97 checksum validates.
static RE_IBAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]){11,30}\b").expect("static iban regex compiles")
});

/// Email addresses. Only applied when `[privacy].scrub_emails = true`
/// (default OFF).
static RE_EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
        .expect("static email regex compiles")
});

/// The privacy filter: scrubbers + path redaction + zone resolution.
///
/// Construct once at boot with [`PrivacyFilter::from_config`] and share
/// (it is cheap to clone). All methods are pure and deterministic.
///
/// **Contract:** [`PrivacyFilter::scrub_text`] is for free-text fields
/// only; structured fields are exempt *by construction* (callers never
/// scrub them). See the module docs.
#[derive(Debug, Clone)]
pub struct PrivacyFilter {
    scrub_api_keys: bool,
    scrub_cards: bool,
    scrub_iban: bool,
    scrub_emails: bool,
    zone_rules: Vec<ZoneRule>,
    home_dir: Option<String>,
    username: Option<String>,
}

impl PrivacyFilter {
    /// Builds the filter from config, performing the **load-time legacy
    /// synthesis** (spec §4.1): the built-in private-browsing keywords
    /// ([`PRIVATE_BROWSING_KEYWORDS`]) become `local_only` rules, legacy
    /// `[context].sensitive_process_names` become `never_observe` rules,
    /// legacy `[context].sensitive_title_keywords` become `local_only`
    /// rules, and explicit `[privacy].zones` rules are unioned in. Overlaps
    /// need no dedup because [`PrivacyFilter::resolve_zone`] takes the
    /// strictest matching zone — stricter always wins.
    ///
    /// The home directory and username for [`PrivacyFilter::scrub_path`]
    /// are detected from the environment; override them with
    /// [`PrivacyFilter::with_environment`] (tests, embedders).
    pub fn from_config(context: &ContextConfig, privacy: &PrivacyConfig) -> Self {
        let mut zone_rules = Vec::new();
        for keyword in PRIVATE_BROWSING_KEYWORDS {
            zone_rules.push(ZoneRule {
                match_process: None,
                match_title_keyword: Some(keyword.to_string()),
                zone: Zone::LocalOnly,
            });
        }
        for process in &context.sensitive_process_names {
            zone_rules.push(ZoneRule {
                match_process: Some(process.clone()),
                match_title_keyword: None,
                zone: Zone::NeverObserve,
            });
        }
        for keyword in &context.sensitive_title_keywords {
            zone_rules.push(ZoneRule {
                match_process: None,
                match_title_keyword: Some(keyword.clone()),
                zone: Zone::LocalOnly,
            });
        }
        zone_rules.extend(privacy.zones.iter().cloned());

        let home_dir = dirs::home_dir();
        let username = home_dir
            .as_ref()
            .and_then(|home| home.file_name())
            .map(|name| name.to_string_lossy().into_owned());
        Self {
            scrub_api_keys: privacy.scrub_api_keys,
            scrub_cards: privacy.scrub_cards,
            scrub_iban: privacy.scrub_iban,
            scrub_emails: privacy.scrub_emails,
            zone_rules,
            home_dir: home_dir.map(|home| home.to_string_lossy().into_owned()),
            username,
        }
    }

    /// Overrides the detected home directory and username used by
    /// [`PrivacyFilter::scrub_path`]. `None` disables the corresponding
    /// redaction. Intended for tests and embedders with non-standard
    /// environments.
    pub fn with_environment(mut self, home_dir: Option<String>, username: Option<String>) -> Self {
        self.home_dir = home_dir;
        self.username = username;
        self
    }

    /// The synthesised zone rule set (built-ins + legacy + explicit), in
    /// evaluation order. Exposed for tests and the dashboard.
    pub fn zone_rules(&self) -> &[ZoneRule] {
        &self.zone_rules
    }

    /// Scrubs secrets from a **free-text** field. Idempotent.
    ///
    /// # Contract — free text ONLY
    ///
    /// Call this on titles, captions, transcripts, summaries, and commit
    /// *subjects*. Do **not** call it on structured fields (git commit
    /// ids, branch names, frame ids, dedupe keys) — those are exempt from
    /// scrubbing *by construction*, meaning the exemption is that this
    /// function is never invoked on them. In free text, high-entropy
    /// strings that happen to be git OIDs, UUIDs, or sha256 digests **are
    /// redacted on purpose**: prose gives no way to distinguish them from
    /// leaked tokens.
    ///
    /// Pattern families and their `[privacy]` toggles:
    /// - `scrub_api_keys` (also gates the generic entropy patterns):
    ///   `Bearer …`, `sk-…`, GitHub `gh?_…`, GitLab `glpat-…`, Slack
    ///   `xox?-…`, Google `ya29.…`, JWTs, `AKIA…`, UUIDs, hex runs
    ///   ≥ 32 chars, base64 **and base64url** runs ≥ 32 chars with
    ///   character diversity.
    /// - `scrub_cards`: 13-19 digit runs passing the Luhn check.
    /// - `scrub_iban`: IBAN candidates passing the mod-97 check.
    /// - `scrub_emails` (default OFF): email addresses.
    ///
    /// Replacement literal: [`REDACTED`].
    pub fn scrub_text(&self, text: &str) -> String {
        let mut out = text.to_string();
        if self.scrub_api_keys {
            out = RE_BEARER
                .replace_all(&out, format!("Bearer {REDACTED}").as_str())
                .into_owned();
            out = RE_SK_KEY
                .replace_all(&out, |caps: &Captures<'_>| {
                    // Real keys contain digits; prose like "sk-learn" does not.
                    if caps[0].bytes().any(|b| b.is_ascii_digit()) {
                        REDACTED.to_string()
                    } else {
                        caps[0].to_string()
                    }
                })
                .into_owned();
            out = RE_GITHUB_TOKEN.replace_all(&out, REDACTED).into_owned();
            // Vendor-shaped tokens run *before* the generic base64 rule so
            // the redaction is one span, not a chopped-up remainder.
            out = RE_GITLAB_TOKEN.replace_all(&out, REDACTED).into_owned();
            out = RE_SLACK_TOKEN.replace_all(&out, REDACTED).into_owned();
            out = RE_GOOGLE_OAUTH.replace_all(&out, REDACTED).into_owned();
            out = RE_JWT.replace_all(&out, REDACTED).into_owned();
            out = RE_AKIA.replace_all(&out, REDACTED).into_owned();
            out = RE_UUID.replace_all(&out, REDACTED).into_owned();
            out = RE_HEX32.replace_all(&out, REDACTED).into_owned();
            out = RE_BASE64
                .replace_all(&out, |caps: &Captures<'_>| {
                    let boundary = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let token = &caps[2];
                    if looks_high_entropy(token) {
                        format!("{boundary}{REDACTED}")
                    } else {
                        caps[0].to_string()
                    }
                })
                .into_owned();
        }
        if self.scrub_cards {
            out = RE_CARD
                .replace_all(&out, |caps: &Captures<'_>| {
                    let digits: Vec<u8> = caps[0]
                        .bytes()
                        .filter(u8::is_ascii_digit)
                        .map(|b| b - b'0')
                        .collect();
                    if (13..=19).contains(&digits.len()) && luhn_valid(&digits) {
                        REDACTED.to_string()
                    } else {
                        caps[0].to_string()
                    }
                })
                .into_owned();
        }
        if self.scrub_iban {
            out = RE_IBAN
                .replace_all(&out, |caps: &Captures<'_>| {
                    if iban_valid(&caps[0]) {
                        REDACTED.to_string()
                    } else {
                        caps[0].to_string()
                    }
                })
                .into_owned();
        }
        if self.scrub_emails {
            out = RE_EMAIL.replace_all(&out, REDACTED).into_owned();
        }
        out
    }

    /// Scrubs a filesystem path (always on for cloud-bound and persisted
    /// paths): the home-directory prefix becomes `~`, and any remaining
    /// path component equal to the username (case-insensitive) becomes
    /// [`REDACTED`]. Structure (separators, other components, extension)
    /// is preserved. Idempotent.
    ///
    /// This is a *structured-field* scrubber: it never applies the secret
    /// scrubbers, so a path containing a 40-char hex directory name
    /// survives untouched apart from home/username redaction.
    pub fn scrub_path(&self, path: &str) -> String {
        let mut out = path.to_string();
        if let Some(home) = &self.home_dir {
            if !home.is_empty() && out.len() >= home.len() && out.is_char_boundary(home.len()) {
                let (prefix, rest) = out.split_at(home.len());
                if normalize_path_for_compare(prefix) == normalize_path_for_compare(home)
                    && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
                {
                    out = format!("~{rest}");
                }
            }
        }
        if let Some(user) = &self.username {
            if !user.is_empty() {
                out = redact_path_component(&out, user);
            }
        }
        out
    }

    /// Resolves the [`Zone`] for a foreground process + window title
    /// against the synthesised rule set. All matching rules are combined
    /// with [`strictest`] — stricter always wins on overlap. No matching
    /// rule yields [`Zone::CloudAllowed`].
    ///
    /// Inputs are matched only, never mutated: this function is safe for
    /// structured fields.
    pub fn resolve_zone(&self, process_name: &str, window_title: &str) -> Zone {
        strictest(
            self.zone_rules
                .iter()
                .filter(|rule| rule.matches(process_name, window_title))
                .map(|rule| rule.zone),
        )
    }

    /// Convenience interop: the zone for a process/title expressed as the
    /// legacy [`PrivacyDisposition`] (see the [`From<Zone>`] mapping).
    pub fn disposition(&self, process_name: &str, window_title: &str) -> PrivacyDisposition {
        self.resolve_zone(process_name, window_title).into()
    }
}

/// Character-diversity gate for base64 candidates: random tokens virtually
/// always mix lowercase, uppercase, and digits; long plain words and
/// shouted words do not.
fn looks_high_entropy(token: &str) -> bool {
    let has_lower = token.bytes().any(|b| b.is_ascii_lowercase());
    let has_upper = token.bytes().any(|b| b.is_ascii_uppercase());
    let has_digit = token.bytes().any(|b| b.is_ascii_digit());
    has_lower && has_upper && has_digit
}

/// Luhn checksum over a digit slice (most-significant first).
fn luhn_valid(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for &d in digits.iter().rev() {
        let mut v = u32::from(d);
        if double {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
        double = !double;
    }
    sum.is_multiple_of(10)
}

/// IBAN mod-97 validation (ISO 13616) over a candidate that may contain
/// single spaces.
fn iban_valid(candidate: &str) -> bool {
    let compact: String = candidate.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 15 || compact.len() > 34 {
        return false;
    }
    let bytes = compact.as_bytes();
    if !(bytes[0].is_ascii_uppercase()
        && bytes[1].is_ascii_uppercase()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit())
    {
        return false;
    }
    // Move the country code + check digits to the end, then compute the
    // big-number remainder mod 97 incrementally (A=10 … Z=35).
    let rearranged = format!("{}{}", &compact[4..], &compact[..4]);
    let mut rem = 0u32;
    for c in rearranged.chars() {
        let v = if c.is_ascii_digit() {
            c as u32 - '0' as u32
        } else if c.is_ascii_uppercase() {
            c as u32 - 'A' as u32 + 10
        } else {
            return false;
        };
        rem = if v < 10 {
            (rem * 10 + v) % 97
        } else {
            (rem * 100 + v) % 97
        };
    }
    rem == 1
}

/// Normalises a path fragment for comparison: backslashes become forward
/// slashes and ASCII is lowercased. Byte length is preserved, so byte
/// offsets computed on the normalised form are valid on the original.
fn normalize_path_for_compare(fragment: &str) -> String {
    fragment
        .chars()
        .map(|c| {
            if c == '\\' {
                '/'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

/// Replaces every path component equal to `user` (case-insensitive) with
/// [`REDACTED`], preserving separators and all other components.
fn redact_path_component(path: &str, user: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut component = String::new();
    for c in path.chars() {
        if c == '/' || c == '\\' {
            push_component(&mut out, &component, user);
            out.push(c);
            component.clear();
        } else {
            component.push(c);
        }
    }
    push_component(&mut out, &component, user);
    out
}

fn push_component(out: &mut String, component: &str, user: &str) {
    if component.eq_ignore_ascii_case(user) {
        out.push_str(REDACTED);
    } else {
        out.push_str(component);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PrivacyConfig;

    fn test_filter() -> PrivacyFilter {
        PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
            .with_environment(
                Some("C:\\Users\\testuser".to_string()),
                Some("testuser".to_string()),
            )
    }

    // --- Secret corpus: secrets in → clean out ---

    #[test]
    fn secret_corpus_is_scrubbed() {
        let filter = test_filter();
        let corpus: &[(&str, &str)] = &[
            (
                "key sk-ant-api03-AbCdEf0123456789AbCdEf0123456789 leaked",
                "sk-ant-api03",
            ),
            (
                "token ghp_AbCd1234EfGh5678IjKl9012MnOp3456QrSt in log",
                "ghp_",
            ),
            ("aws AKIAIOSFODNN7EXAMPLE in env", "AKIAIOSFODNN7EXAMPLE"),
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.payload",
                "eyJhbGci",
            ),
            (
                "digest e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 shown",
                "e3b0c442",
            ),
            (
                "session Zx9kQ2mP8vL4nR7tW1yB5cD3fG6hJ0aE expired",
                "Zx9kQ2mP",
            ),
            ("card 4111 1111 1111 1111 charged", "4111"),
            ("transfer to NL91ABNA0417164300 done", "NL91ABNA"),
        ];
        for (input, fragment) in corpus {
            let out = filter.scrub_text(input);
            assert!(
                !out.contains(fragment),
                "secret fragment {fragment:?} survived: {out:?}"
            );
            assert!(out.contains(REDACTED), "no redaction marker in {out:?}");
        }
    }

    #[test]
    fn bearer_scrub_keeps_scheme_word() {
        let filter = test_filter();
        let out = filter.scrub_text("Authorization: Bearer abcdefghij1234567890XYZ");
        assert_eq!(out, "Authorization: Bearer [REDACTED]");
    }

    #[test]
    fn email_scrub_is_flag_gated() {
        let context = ContextConfig::default();
        let off = PrivacyFilter::from_config(&context, &PrivacyConfig::default());
        assert_eq!(
            off.scrub_text("mail toshan@example.com now"),
            "mail toshan@example.com now",
            "emails must survive with the default (off) flag"
        );
        let privacy = PrivacyConfig {
            scrub_emails: true,
            ..PrivacyConfig::default()
        };
        let on = PrivacyFilter::from_config(&context, &privacy);
        assert_eq!(
            on.scrub_text("mail toshan@example.com now"),
            format!("mail {REDACTED} now")
        );
    }

    #[test]
    fn scrubber_toggles_disable_families() {
        let context = ContextConfig::default();
        let privacy = PrivacyConfig {
            scrub_api_keys: false,
            scrub_cards: false,
            scrub_iban: false,
            scrub_emails: false,
            ..PrivacyConfig::default()
        };
        let filter = PrivacyFilter::from_config(&context, &privacy);
        let input = "sk-abc123def456ghi789 and 4111 1111 1111 1111 and NL91ABNA0417164300";
        assert_eq!(filter.scrub_text(input), input);
    }

    // --- Entropy rule in free text: OIDs/UUIDs/sha256 DO get redacted ---

    #[test]
    fn entropy_rule_redacts_oid_uuid_sha256_in_free_text() {
        // Documented and intended: in FREE TEXT a git OID, a UUID, and a
        // sha256 digest are indistinguishable from leaked tokens and are
        // redacted. Structured fields never pass through scrub_text — see
        // structured_fields_are_exempt_by_construction below.
        let filter = test_filter();
        let oid = "3f785ea1b9c2d4e6a8b0c2d4e6f8a0b2c4d6e8f0";
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        for token in [oid, uuid, sha256] {
            let out = filter.scrub_text(&format!("see commit {token} for details"));
            assert!(!out.contains(token), "{token} survived free-text scrub");
            assert!(out.contains(REDACTED));
        }
    }

    // --- False-positive corpus: structured fields survive by construction ---

    #[test]
    fn structured_fields_are_exempt_by_construction() {
        // A collector holding structured git facts scrubs ONLY the free-text
        // subject; the commit id (structured) is passed through without any
        // scrub call — that passthrough IS the contract. The path scrubber
        // and zone resolver, the only filter functions structured values
        // route through, must never mangle them.
        struct GitFacts {
            commit_id: String, // structured — never scrubbed
            subject: String,   // free text — scrubbed
        }
        let filter = test_filter();
        let facts = GitFacts {
            commit_id: "3f785ea1b9c2d4e6a8b0c2d4e6f8a0b2c4d6e8f0".to_string(),
            subject: "fix auth: stop logging sk-live12345678901234 keys".to_string(),
        };
        // Collector emit: subject through scrub_text, commit id passthrough.
        let emitted_subject = filter.scrub_text(&facts.subject);
        let emitted_commit = facts.commit_id.clone(); // no scrub call, by construction
        assert!(!emitted_subject.contains("sk-live"));
        assert_eq!(
            emitted_commit, "3f785ea1b9c2d4e6a8b0c2d4e6f8a0b2c4d6e8f0",
            "structured commit id must survive end-to-end"
        );

        // scrub_path never applies secret scrubbers: a 40-hex directory
        // name (e.g. an object-store path) survives.
        let path = "D:\\repo\\.git\\objects\\3f\\785ea1b9c2d4e6a8b0c2d4e6f8a0b2c4d6e8f0aa";
        assert_eq!(filter.scrub_path(path), path);

        // resolve_zone matches, never mutates: a UUID in a title is intact
        // input; the zone decision alone comes back.
        let title = "job 550e8400-e29b-41d4-a716-446655440000 finished";
        assert_eq!(filter.resolve_zone("code.exe", title), Zone::CloudAllowed);
    }

    #[test]
    fn false_positive_corpus_survives_free_text_scrub() {
        let filter = test_filter();
        let survivors = [
            "the fix landed in commit deadbee yesterday", // short hex
            "install the sk-learn-pipeline-config package", // sk- prose, no digit
            "Bearer of good news arrived today",          // Bearer prose
            "invoice number 1234 5678 9012 3456 attached", // Luhn-invalid
            "SUPERCALIFRAGILISTICEXPIALIDOCIOUSLY long word here", // no diversity
            "mail toshan@example.com now",                // emails default off
        ];
        for input in survivors {
            assert_eq!(
                filter.scrub_text(input),
                input,
                "false positive on {input:?}"
            );
        }
    }

    // --- fixwave 2 (M5): base64url + vendor token shapes -------------------

    /// The base64 rule used the standard alphabet only (`+/`), so a
    /// base64url payload broke at its first `-`/`_` — often leaving every
    /// fragment under the 32-char floor, i.e. fully unredacted.
    #[test]
    fn base64url_payloads_are_redacted() {
        let filter = test_filter();
        let leaks = [
            // base64url, hyphen at position 12 — the old rule saw a 12-char
            // head and a 30-char tail and redacted neither.
            "token AbCdEf012345-9ZyXwVuTsRqPoNmLkJiHgFeDcBa0123456 leaked",
            // underscore variant
            "payload QWxhZGRpbjpvcGVuc2V6YW1l_Zm9vYmFyYmF6cXV4MDEyMzQ1 here",
            // The shape a pasted secret actually arrives in — `=` must be a
            // usable boundary or this is unmatchable.
            "export TOKEN=AbCdEf012345-9ZyXwVuTsRqPoNmLkJiHgFeDcBa0123456 # pasted",
        ];
        for input in leaks {
            let out = filter.scrub_text(input);
            assert!(
                out.contains(REDACTED),
                "base64url payload survived: {input:?} -> {out:?}"
            );
            assert!(!out.contains("0123456"), "{out:?}");
        }
    }

    #[test]
    fn vendor_token_shapes_are_redacted() {
        let filter = test_filter();
        let leaks = [
            // Slack
            concat!(
                "posting with ",
                "xoxb-",
                "2493847592-2493847598-AbCdEfGhIjKlMnOpQrStUvWx now"
            ),
            concat!(
                "user token ",
                "xoxp-",
                "1234567890-1234567890-1234567890-abcdef1234567890"
            ),
            // GitLab
            concat!("clone with ", "glpat-", "AbCdEf01234567890xyz please"),
            // Google OAuth
            "ya29.a0ARrdaM-9kQwErTyUiOpAsDfGhJkLzXcVbNm1234567890qwerty passed",
            // JWT (three base64url segments)
            "Authorization eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dQw4w9WgXcQ1234567890",
        ];
        for input in leaks {
            let out = filter.scrub_text(input);
            assert!(out.contains(REDACTED), "not redacted: {input:?} -> {out:?}");
        }
        // Each vendor shape must be gone by *name*, not merely mangled:
        // the prefix is itself the tell that a secret was there.
        for (needle, sample) in [
            (
                "xoxb-",
                concat!("xoxb-", "2493847592-2493847598-AbCdEfGhIjKlMnOpQrStUvWx"),
            ),
            (
                "xoxp-",
                concat!("xoxp-", "1234567890-1234567890-1234567890-abcdef1234567890"),
            ),
            ("glpat-", concat!("glpat-", "AbCdEf01234567890xyz")),
            (
                "ya29.",
                "ya29.a0ARrdaM-9kQwErTyUiOpAsDfGhJkLzXcVbNm1234567890",
            ),
            (
                "eyJhbGciOi",
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dQw4w9WgXcQ",
            ),
        ] {
            let out = filter.scrub_text(&format!("prefix {sample} suffix"));
            assert!(!out.contains(needle), "{needle} survived: {out:?}");
            assert!(
                out.starts_with("prefix ") && out.ends_with(" suffix"),
                "{out:?}"
            );
        }
    }

    /// The new rules must not eat the false-positive corpus. Structured
    /// fields (git OIDs, UUIDs, sha256) are exempt *by construction* —
    /// `scrub_path`/`resolve_zone` never invoke the secret scrubbers — and
    /// that has to stay true now that the base64 alphabet includes `-`/`_`.
    #[test]
    fn base64url_widening_does_not_touch_structured_fields() {
        let filter = test_filter();
        let structured = [
            // git OID as an object-store path component
            "D:\\repo\\.git\\objects\\3f\\785ea1b9c2d4e6a8b0c2d4e6f8a0b2c4d6e8f0aa",
            // UUID-named artifact directory
            "~/cache/550e8400-e29b-41d4-a716-446655440000/blob.bin",
            // sha256-named file
            "~/store/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855.tar",
            // long snake_case path, now inside the widened alphabet
            "~/src/very_long_module_name_with_underscores_everywhere/mod.rs",
        ];
        for path in structured {
            assert_eq!(filter.scrub_path(path), path, "mangled {path:?}");
        }

        // …and prose that merely happens to be long stays intact.
        for prose in [
            "the very-long-kebab-case-branch-name-for-this-change landed",
            "see docs/superpowers/specs/2026-08-05-context-engine.md for the rules",
            "run cargo test -p continuum-core --no-default-features --lib now",
        ] {
            assert_eq!(
                filter.scrub_text(prose),
                prose,
                "false positive on {prose:?}"
            );
        }
    }

    // --- Idempotence ---

    #[test]
    fn scrub_text_is_idempotent() {
        let filter = test_filter();
        let inputs = [
            "key sk-ant-api03-AbCdEf0123456789AbCdEf0123456789 leaked",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "digest e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "card 4111 1111 1111 1111 charged to NL91ABNA0417164300",
            "uuid 550e8400-e29b-41d4-a716-446655440000 plain text stays",
            // fixwave 2 (M5) shapes
            concat!(
                "slack ",
                "xoxb-",
                "2493847592-2493847598-AbCdEfGhIjKlMnOpQrStUvWx"
            ),
            concat!("gitlab ", "glpat-", "AbCdEf01234567890xyz"),
            "google ya29.a0ARrdaM-9kQwErTyUiOpAsDfGhJkLzXcVbNm1234567890",
            "jwt eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dQw4w9WgXcQ",
            "b64url AbCdEf012345-9ZyXwVuTsRqPoNmLkJiHgFeDcBa0123456",
        ];
        for input in inputs {
            let once = filter.scrub_text(input);
            let twice = filter.scrub_text(&once);
            assert_eq!(once, twice, "scrub not idempotent for {input:?}");
        }
    }

    #[test]
    fn scrub_path_is_idempotent() {
        let filter = test_filter();
        let path = "C:\\Users\\testuser\\projects\\testuser\\main.rs";
        let once = filter.scrub_path(path);
        assert_eq!(once, filter.scrub_path(&once));
    }

    // --- scrub_path ---

    #[test]
    fn scrub_path_replaces_home_prefix_and_username() {
        let filter = test_filter();
        assert_eq!(
            filter.scrub_path("C:\\Users\\testuser\\projects\\x.rs"),
            "~\\projects\\x.rs"
        );
        assert_eq!(
            filter.scrub_path("C:/Users/testuser/projects"),
            "~/projects"
        );
        // Case-insensitive home match (Windows paths).
        assert_eq!(filter.scrub_path("c:\\users\\TESTUSER\\x"), "~\\x");
        // Exact home dir alone.
        assert_eq!(filter.scrub_path("C:\\Users\\testuser"), "~");
        // Username outside the home prefix is redacted anywhere,
        // structure preserved.
        assert_eq!(
            filter.scrub_path("D:\\backup\\testuser\\file.txt"),
            format!("D:\\backup\\{REDACTED}\\file.txt")
        );
        // A component merely containing the username is NOT redacted
        // (component equality, not substring).
        assert_eq!(
            filter.scrub_path("D:\\testuser-old\\file.txt"),
            "D:\\testuser-old\\file.txt"
        );
        // Unrelated paths untouched.
        assert_eq!(
            filter.scrub_path("D:\\Continuum\\src\\main.rs"),
            "D:\\Continuum\\src\\main.rs"
        );
    }

    #[test]
    fn scrub_path_without_environment_is_passthrough() {
        let filter = test_filter().with_environment(None, None);
        assert_eq!(
            filter.scrub_path("C:\\Users\\testuser\\x.rs"),
            "C:\\Users\\testuser\\x.rs"
        );
    }

    // --- Zones ---

    #[test]
    fn zone_matrix_legacy_synthesis_and_defaults() {
        let filter = test_filter();
        // Legacy sensitive process → never_observe.
        assert_eq!(
            filter.resolve_zone("1password.exe", "1Password"),
            Zone::NeverObserve
        );
        // Legacy title keyword → local_only (case-insensitive substring).
        assert_eq!(
            filter.resolve_zone("chrome.exe", "GitHub — Password reset"),
            Zone::LocalOnly
        );
        // Built-in private-browsing defaults → local_only.
        assert_eq!(
            filter.resolve_zone("msedge.exe", "Docs - InPrivate"),
            Zone::LocalOnly
        );
        assert_eq!(
            filter.resolve_zone("chrome.exe", "New Incognito tab"),
            Zone::LocalOnly
        );
        assert_eq!(
            filter.resolve_zone("firefox.exe", "Mozilla Firefox Private Browsing"),
            Zone::LocalOnly
        );
        // No rule → cloud_allowed.
        assert_eq!(
            filter.resolve_zone("code.exe", "main.rs — Continuum"),
            Zone::CloudAllowed
        );
    }

    #[test]
    fn stricter_zone_wins_on_overlap() {
        let context = ContextConfig::default();
        let privacy = PrivacyConfig {
            zones: vec![
                // Explicit cloud_allowed for a legacy never_observe process:
                // the union applies and stricter (never_observe) wins.
                ZoneRule {
                    match_process: Some("1password.exe".to_string()),
                    match_title_keyword: None,
                    zone: Zone::CloudAllowed,
                },
                // Explicit never_observe by title keyword escalates over the
                // legacy local_only keyword rule.
                ZoneRule {
                    match_process: None,
                    match_title_keyword: Some("seed phrase".to_string()),
                    zone: Zone::NeverObserve,
                },
            ],
            ..PrivacyConfig::default()
        };
        let filter = PrivacyFilter::from_config(&context, &privacy);
        assert_eq!(
            filter.resolve_zone("1password.exe", "vault"),
            Zone::NeverObserve
        );
        assert_eq!(
            filter.resolve_zone("notepad.exe", "my seed phrase backup"),
            Zone::NeverObserve
        );
    }

    #[test]
    fn zone_rule_requires_all_present_criteria() {
        let rule = ZoneRule {
            match_process: Some("chrome.exe".to_string()),
            match_title_keyword: Some("banking".to_string()),
            zone: Zone::LocalOnly,
        };
        assert!(rule.matches("chrome.exe", "My Banking Portal"));
        assert!(!rule.matches("chrome.exe", "News"));
        assert!(!rule.matches("firefox.exe", "My Banking Portal"));
        // A rule with neither criterion never matches.
        let empty = ZoneRule {
            match_process: None,
            match_title_keyword: None,
            zone: Zone::NeverObserve,
        };
        assert!(!empty.matches("anything.exe", "anything"));
    }

    #[test]
    fn strictest_combines_zones() {
        assert_eq!(strictest([]), Zone::CloudAllowed);
        assert_eq!(strictest([Zone::CloudAllowed]), Zone::CloudAllowed);
        assert_eq!(
            strictest([Zone::CloudAllowed, Zone::LocalOnly]),
            Zone::LocalOnly
        );
        assert_eq!(
            strictest([Zone::LocalOnly, Zone::NeverObserve, Zone::CloudAllowed]),
            Zone::NeverObserve
        );
    }

    #[test]
    fn zone_maps_to_legacy_disposition() {
        assert_eq!(
            PrivacyDisposition::from(Zone::NeverObserve),
            PrivacyDisposition::Excluded
        );
        assert_eq!(
            PrivacyDisposition::from(Zone::LocalOnly),
            PrivacyDisposition::Redacted
        );
        assert_eq!(
            PrivacyDisposition::from(Zone::CloudAllowed),
            PrivacyDisposition::Visible
        );
        let filter = test_filter();
        assert_eq!(
            filter.disposition("1password.exe", ""),
            PrivacyDisposition::Excluded
        );
        assert_eq!(
            filter.disposition("code.exe", "main.rs"),
            PrivacyDisposition::Visible
        );
    }

    #[test]
    fn zone_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&Zone::NeverObserve).unwrap(),
            "\"never_observe\""
        );
        assert_eq!(
            serde_json::to_string(&Zone::LocalOnly).unwrap(),
            "\"local_only\""
        );
        assert_eq!(
            serde_json::to_string(&Zone::CloudAllowed).unwrap(),
            "\"cloud_allowed\""
        );
        let z: Zone = serde_json::from_str("\"never_observe\"").unwrap();
        assert_eq!(z, Zone::NeverObserve);
    }

    #[test]
    fn legacy_synthesis_produces_expected_rules() {
        let filter = test_filter();
        let rules = filter.zone_rules();
        // Built-in private browsing rules present.
        for kw in PRIVATE_BROWSING_KEYWORDS {
            assert!(
                rules
                    .iter()
                    .any(|r| r.match_title_keyword.as_deref() == Some(kw)
                        && r.zone == Zone::LocalOnly),
                "missing private-browsing rule for {kw}"
            );
        }
        // Every legacy sensitive process became a never_observe rule.
        for p in &ContextConfig::default().sensitive_process_names {
            assert!(
                rules
                    .iter()
                    .any(|r| r.match_process.as_deref() == Some(p.as_str())
                        && r.zone == Zone::NeverObserve),
                "missing never_observe rule for {p}"
            );
        }
        // Every legacy title keyword became a local_only rule.
        for k in &ContextConfig::default().sensitive_title_keywords {
            assert!(
                rules
                    .iter()
                    .any(|r| r.match_title_keyword.as_deref() == Some(k.as_str())
                        && r.zone == Zone::LocalOnly),
                "missing local_only rule for {k}"
            );
        }
    }

    #[test]
    fn sentinel_constants_are_stable() {
        // These literals are part of the spec §4.1 sentinel contract;
        // downstream consumers (session state, salience, events) key on
        // them. Changing them is a schema change.
        assert_eq!(EXCLUDED_PROCESS, "[excluded]");
        assert_eq!(EXCLUDED_TITLE, "");
        assert_eq!(PRIVATE_LABEL, "[private]");
        assert_eq!(REDACTED, "[REDACTED]");
        // The redaction literal itself never re-matches any scrubber.
        let filter = test_filter();
        assert_eq!(filter.scrub_text(REDACTED), REDACTED);
    }

    // --- Honest toggles (spec §4.1) ---

    #[test]
    fn toggles_gate_their_own_source_only() {
        let toggles = ObservationToggles {
            mic: false,
            screen: true,
            files: true,
            git: true,
            pause_all: false,
        };
        assert!(!source_enabled(&toggles, ObservedSource::Mic));
        assert!(source_enabled(&toggles, ObservedSource::Screen));
        assert!(source_enabled(&toggles, ObservedSource::Files));
        assert!(source_enabled(&toggles, ObservedSource::Git));
        assert!(source_enabled(&toggles, ObservedSource::Window));
    }

    #[test]
    fn pause_all_gates_every_source() {
        let toggles = ObservationToggles {
            mic: true,
            screen: true,
            files: true,
            git: true,
            pause_all: true,
        };
        for source in [
            ObservedSource::Mic,
            ObservedSource::Screen,
            ObservedSource::Files,
            ObservedSource::Git,
            ObservedSource::Window,
        ] {
            assert!(
                !source_enabled(&toggles, source),
                "{source:?} must be gated by pause_all"
            );
        }
    }

    #[test]
    fn default_toggles_allow_everything() {
        let toggles = ObservationToggles::default();
        for source in [
            ObservedSource::Mic,
            ObservedSource::Screen,
            ObservedSource::Files,
            ObservedSource::Git,
            ObservedSource::Window,
        ] {
            assert!(source_enabled(&toggles, source));
        }
    }

    #[test]
    fn iban_and_luhn_validators() {
        assert!(iban_valid("NL91ABNA0417164300"));
        assert!(iban_valid("NL91 ABNA 0417 1643 00"));
        assert!(!iban_valid("NL92ABNA0417164300")); // bad check digits
        assert!(!iban_valid("XX00SHORT"));
        assert!(luhn_valid(&[
            4, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1
        ]));
        assert!(!luhn_valid(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6
        ]));
    }
}
