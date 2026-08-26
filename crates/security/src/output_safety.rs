//! Non-TUI control-output filter for tool/process log exports and metadata.
//!
//! Neutralizes OSC (8/52/title), CSI, DCS/APC/PM/SOS, C0/C1, and bidi format
//! controls so `rapid jobs logs` and artifact metadata cannot trigger terminal
//! side effects. JSON exports stay valid UTF-8. Threat: `T-013`.
//!
//! Documented whitespace that may remain: TAB (`U+0009`) and LF (`U+000A`).
//! CR is rewritten to LF and is never emitted raw.

use std::borrow::Cow;
use std::fmt::{self, Debug, Display, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// CSI / nF sequences longer than this are truncated and the tail is re-scanned.
pub const MAX_CONTROL_SEQUENCE_BYTES: usize = 4096;

/// Maximum accepted bytes for one jobs-logs / tool / process export.
pub const MAX_LOG_EXPORT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum accepted UTF-8 bytes for one artifact metadata field.
pub const MAX_ARTIFACT_METADATA_FIELD_BYTES: usize = 256;

const CANCEL_CHECK_STRIDE: usize = 4096;
const ESC: char = '\u{001B}';
const BEL: char = '\u{0007}';
const DEL: char = '\u{007F}';
const CSI_8BIT: char = '\u{009B}';
const ST_8BIT: char = '\u{009C}';
const OSC_8BIT: char = '\u{009D}';
const DCS_8BIT: char = '\u{0090}';
const SOS_8BIT: char = '\u{0098}';
const PM_8BIT: char = '\u{009E}';
const APC_8BIT: char = '\u{009F}';

/// Sink the filter is protecting. Attribution only; never grants privilege.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OutputChannel {
    JobsLogs,
    ToolLog,
    ProcessLog,
    ArtifactMetadata,
    JsonExport,
}

/// Outcome of a completed filter. Error/unavailable never become this.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OutputSafetyStatus {
    Clean,
    Sanitized,
}

/// Cooperative cancellation for bounded log/metadata filtering.
#[derive(Clone, Debug)]
pub struct OutputSafetyCancellation {
    cancelled: Arc<AtomicBool>,
}

/// Typed filter failure. Messages never include attacker payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputSafetyError {
    Cancelled,
    BoundExceeded { limit: usize, requested: usize },
    Unavailable,
    Closed,
}

/// Filtered UTF-8 text plus status. Debug omits payload bytes.
pub struct SafeOutput {
    channel: OutputChannel,
    status: OutputSafetyStatus,
    text: String,
}

/// Holds an incomplete UTF-8 suffix so log chunks stay decodable.
///
/// Incomplete OSC/CSI is not held: each decoded chunk is neutralized
/// independently so a split sequence cannot reassemble.
pub struct StreamingOutputFilter {
    channel: OutputChannel,
    pending_utf8: Vec<u8>,
    bytes_in: usize,
    state: StreamState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamState {
    Open,
    Finished,
    Failed,
}

impl OutputChannel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JobsLogs => "jobs_logs",
            Self::ToolLog => "tool_log",
            Self::ProcessLog => "process_log",
            Self::ArtifactMetadata => "artifact_metadata",
            Self::JsonExport => "json_export",
        }
    }

    pub const fn max_bytes(self) -> usize {
        match self {
            Self::ArtifactMetadata => MAX_ARTIFACT_METADATA_FIELD_BYTES,
            Self::JobsLogs | Self::ToolLog | Self::ProcessLog | Self::JsonExport => {
                MAX_LOG_EXPORT_BYTES
            }
        }
    }
}

impl OutputSafetyStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Sanitized => "sanitized",
        }
    }
}

impl OutputSafetyCancellation {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), OutputSafetyError> {
        if self.is_cancelled() {
            Err(OutputSafetyError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for OutputSafetyCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputSafetyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "security.output_safety_cancelled",
            Self::BoundExceeded { .. } => "security.output_safety_bound_exceeded",
            Self::Unavailable => "security.output_safety_unavailable",
            Self::Closed => "security.output_safety_closed",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for OutputSafetyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("output safety filter was cancelled"),
            Self::BoundExceeded { limit, requested } => write!(
                f,
                "output safety payload exceeds bound ({requested} > {limit} bytes)"
            ),
            Self::Unavailable => f.write_str("output safety filter is unavailable"),
            Self::Closed => f.write_str("streaming output filter is closed"),
        }
    }
}

impl std::error::Error for OutputSafetyError {}

impl SafeOutput {
    pub fn channel(&self) -> OutputChannel {
        self.channel
    }

    pub fn status(&self) -> OutputSafetyStatus {
        self.status
    }

    /// Filtered UTF-8. Never contains raw controls except TAB/LF.
    pub fn as_text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }
}

impl Debug for SafeOutput {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SafeOutput")
            .field("channel", &self.channel)
            .field("status", &self.status)
            .field("text_len", &self.text.len())
            .finish()
    }
}

impl StreamingOutputFilter {
    pub fn new(channel: OutputChannel) -> Self {
        Self {
            channel,
            pending_utf8: Vec::new(),
            bytes_in: 0,
            state: StreamState::Open,
        }
    }

    pub fn bytes_in(&self) -> usize {
        self.bytes_in
    }

    pub fn push(
        &mut self,
        bytes: &[u8],
        cancel: &OutputSafetyCancellation,
    ) -> Result<String, OutputSafetyError> {
        self.ensure_open()?;
        if let Err(err) = cancel.check() {
            self.state = StreamState::Failed;
            return Err(err);
        }
        let requested = self.bytes_in.saturating_add(bytes.len());
        if requested > self.channel.max_bytes() {
            self.state = StreamState::Failed;
            return Err(OutputSafetyError::BoundExceeded {
                limit: self.channel.max_bytes(),
                requested,
            });
        }
        self.pending_utf8.extend_from_slice(bytes);
        self.bytes_in = requested;
        let (decoded, replaced) = take_utf8_prefix(&mut self.pending_utf8);
        if decoded.is_empty() {
            return Ok(String::new());
        }
        match filter_decoded(self.channel, &decoded, replaced, cancel) {
            Ok(filtered) => Ok(filtered.text),
            Err(err) => {
                self.state = StreamState::Failed;
                Err(err)
            }
        }
    }

    pub fn finish(
        &mut self,
        cancel: &OutputSafetyCancellation,
    ) -> Result<String, OutputSafetyError> {
        self.ensure_open()?;
        if let Err(err) = cancel.check() {
            self.state = StreamState::Failed;
            return Err(err);
        }
        self.state = StreamState::Finished;
        if self.pending_utf8.is_empty() {
            return Ok(String::new());
        }
        let remaining = std::mem::take(&mut self.pending_utf8);
        let result = match std::str::from_utf8(&remaining) {
            Ok(text) => filter_decoded(self.channel, text, false, cancel),
            Err(_) => {
                let decoded = String::from_utf8_lossy(&remaining);
                filter_decoded(self.channel, decoded.as_ref(), true, cancel)
            }
        };
        match result {
            Ok(filtered) => Ok(filtered.text),
            Err(err) => {
                self.state = StreamState::Failed;
                Err(err)
            }
        }
    }

    fn ensure_open(&self) -> Result<(), OutputSafetyError> {
        match self.state {
            StreamState::Open => Ok(()),
            StreamState::Finished | StreamState::Failed => Err(OutputSafetyError::Closed),
        }
    }
}

impl Debug for StreamingOutputFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamingOutputFilter")
            .field("channel", &self.channel)
            .field("bytes_in", &self.bytes_in)
            .field("pending_utf8_len", &self.pending_utf8.len())
            .field("state", &self.state)
            .finish()
    }
}

/// Preserve printable text and documented whitespace; neutralize terminal controls.
///
/// Cannot emit raw control bytes except TAB and LF. Already-safe input is borrowed.
pub fn safe_text_for_terminal_or_json(input: &str) -> Cow<'_, str> {
    if is_already_safe(input) {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(rewrite(input))
    }
}

/// Bounded, cancellable filter for log exports and artifact metadata.
///
/// Invalid UTF-8 is replaced with `U+FFFD` and reported as [`OutputSafetyStatus::Sanitized`].
/// Cancellation and bound errors never become Clean.
pub fn filter_output(
    channel: OutputChannel,
    bytes: &[u8],
    cancel: &OutputSafetyCancellation,
) -> Result<SafeOutput, OutputSafetyError> {
    cancel.check()?;
    if bytes.len() > channel.max_bytes() {
        return Err(OutputSafetyError::BoundExceeded {
            limit: channel.max_bytes(),
            requested: bytes.len(),
        });
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => filter_decoded(channel, text, false, cancel),
        Err(_) => {
            let lossy = String::from_utf8_lossy(bytes);
            filter_decoded(channel, lossy.as_ref(), true, cancel)
        }
    }
}

/// Sanitize one artifact metadata string field (single-line, bounded).
pub fn safe_artifact_metadata_field(
    input: &str,
    cancel: &OutputSafetyCancellation,
) -> Result<SafeOutput, OutputSafetyError> {
    filter_output(OutputChannel::ArtifactMetadata, input.as_bytes(), cancel)
}

/// Compact JSON object for jobs-logs / tool export. Always valid UTF-8.
///
/// `text` is filtered first; serde failure is [`OutputSafetyError::Unavailable`].
pub fn safe_json_for_export(
    channel: OutputChannel,
    bytes: &[u8],
    cancel: &OutputSafetyCancellation,
) -> Result<String, OutputSafetyError> {
    let filtered = filter_output(channel, bytes, cancel)?;
    cancel.check()?;
    let payload = serde_json::json!({
        "channel": channel.as_str(),
        "status": filtered.status.as_str(),
        "text": filtered.text,
    });
    match serde_json::to_string(&payload) {
        Ok(json) => {
            if std::str::from_utf8(json.as_bytes()).is_err() {
                return Err(OutputSafetyError::Unavailable);
            }
            Ok(json)
        }
        Err(_) => Err(OutputSafetyError::Unavailable),
    }
}

fn filter_decoded(
    channel: OutputChannel,
    text: &str,
    lossy: bool,
    cancel: &OutputSafetyCancellation,
) -> Result<SafeOutput, OutputSafetyError> {
    cancel.check()?;
    check_cancel_over(text, cancel)?;
    let sanitized = safe_text_for_terminal_or_json(text);
    let mut out = sanitized.into_owned();
    if channel == OutputChannel::ArtifactMetadata {
        out = flatten_metadata(&out);
    }
    let status = if lossy || out != text {
        OutputSafetyStatus::Sanitized
    } else {
        OutputSafetyStatus::Clean
    };
    Ok(SafeOutput {
        channel,
        status,
        text: out,
    })
}

fn flatten_metadata(text: &str) -> String {
    text.chars().filter(|c| *c != '\t' && *c != '\n').collect()
}

fn check_cancel_over(
    text: &str,
    cancel: &OutputSafetyCancellation,
) -> Result<(), OutputSafetyError> {
    let mut scanned = 0usize;
    for _ in text.chars() {
        scanned = scanned.saturating_add(1);
        if scanned.is_multiple_of(CANCEL_CHECK_STRIDE) {
            cancel.check()?;
        }
    }
    cancel.check()
}

fn is_already_safe(input: &str) -> bool {
    input.chars().all(is_passthrough_char)
}

fn is_passthrough_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | ' '..='~') || (c >= '\u{00A0}' && !is_neutralized_format(c))
}

fn is_neutralized_format(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{206A}'..='\u{206F}'
    )
}

fn rewrite(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(c) = rest.chars().next() {
        if c == ESC {
            rest = skip_esc_sequence(rest);
            continue;
        }
        if is_c1(c) {
            rest = skip_c1_sequence(rest, c);
            continue;
        }
        if c == '\r' {
            out.push('\n');
            rest = &rest[1..];
            if rest.starts_with('\n') {
                rest = &rest[1..];
            }
            continue;
        }
        if is_passthrough_char(c) {
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn is_c1(c: char) -> bool {
    matches!(c, '\u{0080}'..='\u{009F}')
}

fn is_string_introducer(c: char) -> bool {
    matches!(c, ']' | 'P' | 'X' | '^' | '_')
}

fn skip_esc_sequence(rest: &str) -> &str {
    let after_esc = &rest[ESC.len_utf8()..];
    let Some(next) = after_esc.chars().next() else {
        return after_esc;
    };
    let after_next = &after_esc[next.len_utf8()..];
    match next {
        '[' => skip_csi_body(after_next),
        c if is_string_introducer(c) => skip_control_string(after_next),
        '\\' => after_next,
        c if is_intermediate_byte(c) => skip_nf_sequence(after_esc),
        c if is_esc_final_byte(c) => after_next,
        _ => after_esc,
    }
}

fn skip_c1_sequence(rest: &str, introducer: char) -> &str {
    let after = &rest[introducer.len_utf8()..];
    match introducer {
        CSI_8BIT => skip_csi_body(after),
        OSC_8BIT | DCS_8BIT | SOS_8BIT | PM_8BIT | APC_8BIT => skip_control_string(after),
        ST_8BIT => after,
        _ => after,
    }
}

fn is_intermediate_byte(c: char) -> bool {
    matches!(c, '\u{0020}'..='\u{002F}')
}

fn is_esc_final_byte(c: char) -> bool {
    matches!(c, '\u{0030}'..='\u{007E}')
}

fn skip_csi_body(mut rest: &str) -> &str {
    let mut consumed = 0usize;
    while let Some(c) = rest.chars().next() {
        let width = c.len_utf8();
        consumed = consumed.saturating_add(width);
        if consumed > MAX_CONTROL_SEQUENCE_BYTES {
            return rest;
        }
        if c == ESC || is_c1(c) {
            return rest;
        }
        let code = c as u32;
        if code < 0x20 || c == DEL {
            rest = &rest[width..];
            continue;
        }
        if (0x20..=0x3F).contains(&code) {
            rest = &rest[width..];
            continue;
        }
        if (0x40..=0x7E).contains(&code) {
            return &rest[width..];
        }
        return rest;
    }
    rest
}

fn skip_nf_sequence(mut rest: &str) -> &str {
    let mut consumed = 0usize;
    let mut saw_intermediate = false;
    while let Some(c) = rest.chars().next() {
        let width = c.len_utf8();
        consumed = consumed.saturating_add(width);
        if consumed > MAX_CONTROL_SEQUENCE_BYTES {
            return rest;
        }
        if is_intermediate_byte(c) {
            saw_intermediate = true;
            rest = &rest[width..];
            continue;
        }
        if saw_intermediate && is_esc_final_byte(c) {
            return &rest[width..];
        }
        return rest;
    }
    rest
}

fn skip_control_string(mut rest: &str) -> &str {
    while let Some(c) = rest.chars().next() {
        if c == BEL || c == ST_8BIT {
            return &rest[c.len_utf8()..];
        }
        if c == ESC {
            let after_esc = &rest[ESC.len_utf8()..];
            if let Some(after_st) = after_esc.strip_prefix('\\') {
                return after_st;
            }
            return rest;
        }
        rest = &rest[c.len_utf8()..];
    }
    rest
}

fn take_utf8_prefix(buf: &mut Vec<u8>) -> (String, bool) {
    let mut out = String::new();
    let mut replaced = false;
    loop {
        match std::str::from_utf8(buf) {
            Ok(s) => {
                out.push_str(s);
                buf.clear();
                return (out, replaced);
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if valid > 0 {
                    match std::str::from_utf8(&buf[..valid]) {
                        Ok(s) => out.push_str(s),
                        Err(_) => {
                            out.push('\u{FFFD}');
                            replaced = true;
                        }
                    }
                }
                match err.error_len() {
                    Some(len) => {
                        let drain_end = valid.saturating_add(len).min(buf.len());
                        if drain_end == 0 {
                            return (out, replaced);
                        }
                        buf.drain(..drain_end);
                        out.push('\u{FFFD}');
                        replaced = true;
                    }
                    None => {
                        buf.drain(..valid);
                        return (out, replaced);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type CorpusCase = (&'static str, &'static str, &'static str);

    /// Same malicious-control corpus as `crates/tui/src/sanitize.rs`.
    const FUZZ_CORPUS: &[CorpusCase] = &[
        ("plain", "hello world", "hello world"),
        ("empty", "", ""),
        ("tab_lf", "a\tb\nc", "a\tb\nc"),
        ("unicode", "hello 世界 🦀", "hello 世界 🦀"),
        (
            "osc8_bel",
            "\u{1b}]8;;https://evil.example/x\u{07}visible\u{1b}]8;;\u{07}",
            "visible",
        ),
        (
            "osc8_st",
            "\u{1b}]8;;https://evil.example/x\u{1b}\\click",
            "click",
        ),
        (
            "osc8_params",
            "\u{1b}]8;id=1:foo=bar;https://phish.test\u{07}lab\u{1b}]8;;\u{07}",
            "lab",
        ),
        (
            "osc52_clipboard",
            "pre\u{1b}]52;c;c2VjcmV0\u{07}post",
            "prepost",
        ),
        ("osc0_title", "\u{1b}]0;pwned-title\u{07}ok", "ok"),
        ("osc1_icon", "\u{1b}]1;icon\u{07}ok", "ok"),
        ("osc2_title", "\u{1b}]2;window\u{07}ok", "ok"),
        ("osc_st8", "\u{1b}]0;title\u{9c}ok", "ok"),
        ("csi_sgr", "\u{1b}[31mred\u{1b}[0m", "red"),
        ("csi_truecolor", "\u{1b}[38:2:255:0:0mrgb", "rgb"),
        ("csi_altscreen_on", "\u{1b}[?1049hsecret", "secret"),
        ("csi_altscreen_off", "keep\u{1b}[?1049l", "keep"),
        ("csi_alt47", "\u{1b}[?47hbody", "body"),
        ("csi_erase", "\u{1b}[2J\u{1b}[Hhome", "home"),
        ("csi_cup", "\u{1b}[10;10Hhere", "here"),
        ("csi_private", "\u{1b}[>cdev", "dev"),
        ("esc_ris", "\u{1b}creset", "reset"),
        ("esc_save_restore", "\u{1b}7x\u{1b}8", "x"),
        ("esc_charset", "\u{1b}(Blatin", "latin"),
        ("esc_dec_aln", "\u{1b}#8fill", "fill"),
        ("esc_utf8", "\u{1b}%Gutf", "utf"),
        ("dcs", "\u{1b}P1$r\u{1b}\\after", "after"),
        ("apc", "\u{1b}_payload\u{07}after", "after"),
        ("pm", "\u{1b}^payload\u{1b}\\after", "after"),
        ("sos", "\u{1b}Xpayload\u{9c}after", "after"),
        ("bel", "a\u{07}b", "ab"),
        ("backspace", "ab\u{08}c", "abc"),
        ("tab_kept", "col\tcol", "col\tcol"),
        ("nul", "a\u{00}b", "ab"),
        ("del", "a\u{7f}b", "ab"),
        ("cr_overwrite", "secret\rpub", "secret\npub"),
        ("crlf", "a\r\nb", "a\nb"),
        ("lone_lf", "a\nb", "a\nb"),
        ("bidi_rlo", "safe\u{202e}ext\u{202c}", "safeext"),
        ("bidi_lre", "a\u{202a}b\u{202c}c", "abc"),
        ("bidi_isolate", "a\u{2066}b\u{2069}c", "abc"),
        ("bidi_rlm", "a\u{200f}b\u{200e}c", "abc"),
        ("bidi_alm", "a\u{061c}b", "ab"),
        ("c1_csi", "\u{9b}31mred", "red"),
        ("c1_osc52", "\u{9d}52;c;QQ==\u{9c}x", "x"),
        ("c1_st_only", "a\u{9c}b", "ab"),
        ("incomplete_esc", "ok\u{1b}", "ok"),
        ("incomplete_csi", "ok\u{1b}[31", "ok"),
        ("incomplete_osc", "ok\u{1b}]52;c;AAAA", "ok"),
        ("incomplete_osc8", "ok\u{1b}]8;;https://x", "ok"),
        ("esc_then_text", "\u{1b}hello", "ello"),
        ("double_esc", "\u{1b}\u{1b}[0mplain", "plain"),
        (
            "mixed_tool_output",
            "out:\u{1b}[32mok\u{1b}[0m \u{1b}]8;;https://x\u{07}link\u{1b}]8;;\u{07}\n\u{202e}bid\u{202c}",
            "out:ok link\nbid",
        ),
    ];

    fn live() -> OutputSafetyCancellation {
        OutputSafetyCancellation::new()
    }

    fn assert_inert(label: &str, text: &str) {
        for (i, c) in text.char_indices() {
            let code = c as u32;
            assert!(
                c == '\t'
                    || c == '\n'
                    || (0x20..=0x7E).contains(&code)
                    || (c >= '\u{00A0}' && !is_neutralized_format(c)),
                "{label}: residual control U+{code:04X} at {i} in {text:?}"
            );
        }
        assert!(!text.contains(ESC), "{label}: residual ESC in {text:?}");
        assert!(!text.contains('\r'), "{label}: residual CR in {text:?}");
        assert!(
            !text.contains(BEL),
            "{label}: residual BEL (OSC terminator / alert) in {text:?}"
        );
        assert!(
            std::str::from_utf8(text.as_bytes()).is_ok(),
            "{label}: not valid UTF-8"
        );
    }

    #[test]
    fn corpus_golden_and_inert_for_terminal_and_jobs_logs() {
        let cancel = live();
        for (name, input, expected) in FUZZ_CORPUS {
            let got = safe_text_for_terminal_or_json(input);
            assert_eq!(got.as_ref(), *expected, "golden {name}");
            assert_inert(name, got.as_ref());

            let logs = filter_output(OutputChannel::JobsLogs, input.as_bytes(), &cancel)
                .expect("jobs logs");
            assert_eq!(logs.as_text(), *expected, "jobs logs golden {name}");
            assert_inert(&format!("jobs-logs-{name}"), logs.as_text());
            assert!(!format!("{logs:?}").contains(input) || is_already_safe(input));
        }
    }

    #[test]
    fn clean_text_is_borrowed() {
        match safe_text_for_terminal_or_json("printable\tline\n世界") {
            Cow::Borrowed(s) => assert_eq!(s, "printable\tline\n世界"),
            Cow::Owned(_) => panic!("clean input must stay borrowed"),
        }
    }

    #[test]
    fn dirty_text_is_owned() {
        match safe_text_for_terminal_or_json("\u{1b}[31mx") {
            Cow::Owned(s) => assert_eq!(s, "x"),
            Cow::Borrowed(_) => panic!("control input must be rewritten"),
        }
    }

    #[test]
    fn sanitize_is_idempotent() {
        for (name, input, _) in FUZZ_CORPUS {
            let once = safe_text_for_terminal_or_json(input);
            let twice = safe_text_for_terminal_or_json(once.as_ref());
            assert_eq!(once.as_ref(), twice.as_ref(), "idempotent {name}");
            assert!(
                matches!(twice, Cow::Borrowed(_)),
                "second pass must borrow {name}"
            );
        }
    }

    #[test]
    fn json_export_is_valid_utf8_and_inert_for_corpus() {
        let cancel = live();
        for (name, input, expected) in FUZZ_CORPUS {
            let json = safe_json_for_export(OutputChannel::JobsLogs, input.as_bytes(), &cancel)
                .expect("json");
            assert!(
                std::str::from_utf8(json.as_bytes()).is_ok(),
                "{name}: json is not valid UTF-8"
            );
            let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");
            let text = parsed
                .get("text")
                .and_then(|v| v.as_str())
                .expect("text field");
            assert_eq!(text, *expected, "json text {name}");
            assert_inert(&format!("json-{name}"), text);
            assert_eq!(
                parsed.get("channel").and_then(|v| v.as_str()),
                Some("jobs_logs")
            );
            if json.as_bytes().contains(&0x1b) {
                panic!("{name}: raw ESC leaked into JSON bytes");
            }
            if json.as_bytes().contains(&0x07) {
                panic!("{name}: raw BEL leaked into JSON bytes");
            }
        }
    }

    #[test]
    fn artifact_metadata_strips_controls_and_newlines() {
        let cancel = live();
        let dirty = "text/\u{1b}]52;c;c2VjcmV0\u{07}plain\twith\nosc";
        let got = safe_artifact_metadata_field(dirty, &cancel).expect("meta");
        assert_eq!(got.status(), OutputSafetyStatus::Sanitized);
        assert_eq!(got.as_text(), "text/plainwithosc");
        assert_inert("artifact-meta", got.as_text());
        assert!(!got.as_text().contains('\t'));
        assert!(!got.as_text().contains('\n'));
        assert_eq!(got.channel(), OutputChannel::ArtifactMetadata);
        assert!(!format!("{got:?}").contains("c2VjcmV0"));
    }

    #[test]
    fn artifact_metadata_bound_fails_closed() {
        let cancel = live();
        let oversized = "a".repeat(MAX_ARTIFACT_METADATA_FIELD_BYTES + 1);
        let err = safe_artifact_metadata_field(&oversized, &cancel).expect_err("bound");
        assert!(matches!(err, OutputSafetyError::BoundExceeded { .. }));
        assert_eq!(err.code(), "security.output_safety_bound_exceeded");
        assert!(!err.retryable());
        assert!(!err.to_string().contains(&oversized));
    }

    #[test]
    fn jobs_logs_bound_fails_closed() {
        let cancel = live();
        let oversized = vec![b'x'; MAX_LOG_EXPORT_BYTES + 1];
        let err = filter_output(OutputChannel::JobsLogs, &oversized, &cancel).expect_err("bound");
        assert!(matches!(
            err,
            OutputSafetyError::BoundExceeded {
                limit: MAX_LOG_EXPORT_BYTES,
                ..
            }
        ));
        assert_eq!(err.code(), "security.output_safety_bound_exceeded");
    }

    #[test]
    fn cancelled_filter_fails_closed() {
        let cancel = OutputSafetyCancellation::new();
        cancel.cancel();
        let err = filter_output(OutputChannel::ProcessLog, b"hello\x1b[31m", &cancel)
            .expect_err("cancelled");
        assert_eq!(err, OutputSafetyError::Cancelled);
        assert_eq!(err.code(), "security.output_safety_cancelled");
        assert!(!format!("{err}").contains("\u{1b}"));
    }

    #[test]
    fn invalid_utf8_becomes_sanitized_valid_utf8() {
        let cancel = live();
        let bytes = [b'o', b'k', 0xff, 0xfe, b'!'];
        let got = filter_output(OutputChannel::ToolLog, &bytes, &cancel).expect("lossy");
        assert_eq!(got.status(), OutputSafetyStatus::Sanitized);
        assert!(std::str::from_utf8(got.as_text().as_bytes()).is_ok());
        assert_inert("lossy-utf8", got.as_text());
        let json = safe_json_for_export(OutputChannel::JsonExport, &bytes, &cancel).expect("json");
        assert!(std::str::from_utf8(json.as_bytes()).is_ok());
        serde_json::from_str::<serde_json::Value>(&json).expect("json parses");
    }

    #[test]
    fn fuzz_inject_complete_controls_around_printable() {
        let payloads: &[&str] = &[
            "\u{1b}]8;;https://evil.test\u{07}",
            "\u{1b}]8;;https://evil.test\u{1b}\\",
            "\u{1b}]52;c;c2VjcmV0\u{07}",
            "\u{1b}]0;title\u{07}",
            "\u{1b}[?1049h",
            "\u{1b}[?1049l",
            "\u{1b}[2J",
            "\u{1b}[31;1m",
            "\u{1b}c",
            "\u{1b}(B",
            "\u{07}",
            "\u{08}",
            "\u{00}",
            "\u{7f}",
            "\u{202e}",
            "\u{202c}",
            "\u{2066}",
            "\u{2069}",
            "\u{9b}0m",
            "\u{9d}52;c;QQ==\u{9c}",
        ];
        let cancel = live();
        for (i, ctrl) in payloads.iter().enumerate() {
            let input = format!("hello{ctrl}world");
            let got = safe_text_for_terminal_or_json(&input);
            assert_inert(&format!("inject-{i}"), got.as_ref());
            assert_eq!(
                got.as_ref(),
                "helloworld",
                "inject-{i}: extra residue {got:?}"
            );
            let logs = filter_output(OutputChannel::JobsLogs, input.as_bytes(), &cancel)
                .expect("jobs logs");
            assert_eq!(logs.as_text(), "helloworld");
        }
    }

    #[test]
    fn fuzz_incomplete_sequences_fail_closed() {
        let payloads: &[&str] = &[
            "\u{1b}",
            "\u{1b}[999",
            "\u{1b}]52;c;OPEN",
            "\u{1b}]8;;https://x",
        ];
        for (i, ctrl) in payloads.iter().enumerate() {
            let input = format!("hello{ctrl}world");
            let got = safe_text_for_terminal_or_json(&input);
            assert_inert(&format!("open-{i}"), got.as_ref());
            assert!(
                got.as_ref() == "hello" || got.starts_with("hello"),
                "open-{i}: lost prefix: {got:?} from {input:?}"
            );
        }
    }

    #[test]
    fn chunked_sanitize_cannot_reassemble_csi() {
        let first = safe_text_for_terminal_or_json("\u{1b}[31");
        let second = safe_text_for_terminal_or_json("mRED");
        let joined = format!("{first}{second}");
        assert_inert("chunked", &joined);
        assert_eq!(joined, "mRED");
    }

    #[test]
    fn streaming_jobs_logs_cannot_reassemble_csi() {
        let cancel = live();
        let mut stream = StreamingOutputFilter::new(OutputChannel::JobsLogs);
        let a = stream.push("\u{1b}[31".as_bytes(), &cancel).expect("first");
        let b = stream.push(b"mRED", &cancel).expect("second");
        let tail = stream.finish(&cancel).expect("finish");
        let joined = format!("{a}{b}{tail}");
        assert_inert("stream-csi", &joined);
        assert_eq!(joined, "mRED");
    }

    #[test]
    fn streaming_holds_incomplete_utf8_then_emits() {
        let cancel = live();
        let mut stream = StreamingOutputFilter::new(OutputChannel::ToolLog);
        let crab = "🦀".as_bytes();
        let first = stream.push(&crab[..1], &cancel).expect("prefix");
        assert!(first.is_empty());
        let rest = stream.push(&crab[1..], &cancel).expect("rest");
        let tail = stream.finish(&cancel).expect("finish");
        assert_eq!(format!("{rest}{tail}"), "🦀");
    }

    #[test]
    fn cancelled_stream_fails_closed_and_stays_closed() {
        let cancel = live();
        let mut stream = StreamingOutputFilter::new(OutputChannel::ProcessLog);
        let _ = stream.push(b"ok", &cancel).expect("push");
        cancel.cancel();
        let err = stream.finish(&cancel).expect_err("cancelled finish");
        assert_eq!(err, OutputSafetyError::Cancelled);
        let closed = stream
            .push(b"more", &live())
            .expect_err("failed stream stays closed");
        assert_eq!(closed, OutputSafetyError::Closed);
        assert!(!format!("{stream:?}").contains("\u{1b}"));
    }

    #[test]
    fn stream_bound_fails_closed() {
        let cancel = live();
        let mut stream = StreamingOutputFilter::new(OutputChannel::ArtifactMetadata);
        let chunk = vec![b'a'; MAX_ARTIFACT_METADATA_FIELD_BYTES];
        stream.push(&chunk, &cancel).expect("fill");
        let err = stream.push(b"x", &cancel).expect_err("over");
        assert!(matches!(err, OutputSafetyError::BoundExceeded { .. }));
        let closed = stream.finish(&live()).expect_err("closed after fail");
        assert_eq!(closed, OutputSafetyError::Closed);
    }

    #[test]
    fn long_csi_is_bounded_and_inert() {
        let mut input = String::from("\u{1b}[");
        input.extend(std::iter::repeat_n('0', MAX_CONTROL_SEQUENCE_BYTES + 32));
        input.push('m');
        input.push_str("tail");
        let got = safe_text_for_terminal_or_json(&input);
        assert_inert("long-csi", got.as_ref());
        assert!(got.ends_with("tail"), "long-csi lost tail: {got:?}");
        assert!(!got.contains(ESC));
    }

    #[test]
    fn unterminated_osc_does_not_leak_introducer() {
        let input = format!("pre\u{1b}]52;c;{}", "A".repeat(128));
        let got = safe_text_for_terminal_or_json(&input);
        assert_eq!(got.as_ref(), "pre");
        assert_inert("open-osc", got.as_ref());
    }

    #[test]
    fn cr_cannot_overwrite_prior_text() {
        let got = safe_text_for_terminal_or_json("password=hunter2\ruser=public");
        assert_eq!(got.as_ref(), "password=hunter2\nuser=public");
        assert_inert("cr-overwrite", got.as_ref());
    }

    #[test]
    fn clean_jobs_logs_status_is_clean() {
        let cancel = live();
        let got = filter_output(OutputChannel::JobsLogs, b"ok line\n", &cancel).expect("filter");
        assert_eq!(got.status(), OutputSafetyStatus::Clean);
        assert_eq!(got.as_text(), "ok line\n");
    }

    #[test]
    fn dirty_jobs_logs_status_is_sanitized_not_clean() {
        let cancel = live();
        let got = filter_output(OutputChannel::JobsLogs, b"\x1b[31mred", &cancel).expect("filter");
        assert_eq!(got.status(), OutputSafetyStatus::Sanitized);
        assert_eq!(got.as_text(), "red");
    }

    #[test]
    fn json_quotes_and_controls_stay_valid() {
        let cancel = live();
        let input = "say \"hi\"\\\u{1b}]52;c;QQ==\u{07}\nnext";
        let json =
            safe_json_for_export(OutputChannel::JobsLogs, input.as_bytes(), &cancel).expect("json");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let text = parsed["text"].as_str().expect("text");
        assert_eq!(text, "say \"hi\"\\\nnext");
        assert_inert("json-quotes", text);
    }
}
