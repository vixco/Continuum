//! Minimal incremental SSE parser: byte chunks in, `data:` payloads out.
//! Handles chunk boundaries mid-line, CRLF, comment lines, and multi-line
//! buffering. Event names are ignored — both the OpenAI and Anthropic
//! streams carry everything we need in the `data:` JSON payload.
//!
//! **UTF-8 safety:** Buffers raw bytes, decoding only complete lines. Since
//! newline (0x0A) never appears inside a multi-byte UTF-8 sequence
//! (continuation bytes are ≥0x80), splitting at `\n` never splits a character.

#![allow(dead_code)] // TODO(task-4): remove — parser gets wired into the HTTP adapters

pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed a network chunk; returns every completed `data:` payload.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        // Process complete lines; keep the trailing partial line in the buffer.
        // Safe to split at \n because it never appears in multi-byte UTF-8 sequences.
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(payload) = line.strip_prefix("data:") {
                out.push(payload.trim().to_string());
            }
            // `event:`, `id:`, comments (`:`), and blank lines are ignored.
        }
        out
    }
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_events() {
        let mut p = SseParser::new();
        let out = p.push(b"event: x\ndata: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
    }

    #[test]
    fn handles_chunks_split_mid_line_and_crlf() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"t\":\"he").is_empty());
        let out = p.push(b"llo\"}\r\n\r\ndata: ");
        assert_eq!(out, vec!["{\"t\":\"hello\"}".to_string()]);
        let out2 = p.push(b"{\"t\":\"world\"}\n\n");
        assert_eq!(out2, vec!["{\"t\":\"world\"}".to_string()]);
    }

    #[test]
    fn ignores_comments_events_and_blank_noise() {
        let mut p = SseParser::new();
        let out = p.push(b": keepalive\n\nevent: ping\n\ndata: 1\n\n");
        assert_eq!(out, vec!["1".to_string()]);
    }

    #[test]
    fn multibyte_utf8_split_across_chunks_survives() {
        let payload = "data: em—dash ✓ ééé\n\n".as_bytes();
        // split at every possible byte position, including mid-character
        for split in 1..payload.len() {
            let mut p = SseParser::new();
            let mut out = p.push(&payload[..split]);
            out.extend(p.push(&payload[split..]));
            assert_eq!(
                out,
                vec!["em—dash ✓ ééé".to_string()],
                "split at byte {split}"
            );
        }
    }
}
