//! Minimal incremental SSE parser: byte chunks in, `data:` payloads out.
//! Handles chunk boundaries mid-line, CRLF, comment lines, and multi-line
//! buffering. Event names are ignored — both the OpenAI and Anthropic
//! streams carry everything we need in the `data:` JSON payload.

pub struct SseParser {
    buf: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Feed a network chunk; returns every completed `data:` payload.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        // Process complete lines; keep the trailing partial line in the buffer.
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
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
}
