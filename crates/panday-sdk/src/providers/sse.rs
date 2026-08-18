//! A minimal Server-Sent Events decoder.
//!
//! Streaming Chat Completions is SSE, and the only field we care about is
//! `data:`. This is deliberately hand-rolled rather than pulled from a crate:
//! it is ~50 lines, it must never panic on hostile input from a provider, and
//! docs/11 puts dialect drift squarely in our court ("We own streaming quirks
//! ... forever").
//!
//! Rules implemented, per the WHATWG SSE grammar:
//!  - a record ends at a blank line;
//!  - multiple `data:` lines within one record join with `\n`;
//!  - a single optional space after the colon is stripped;
//!  - lines beginning with `:` are comments (llama-server sends these as
//!    keep-alives) and are discarded;
//!  - fields we do not use (`event:`, `id:`, `retry:`) are discarded.
//!
//! The decoder is byte-oriented and incremental: network chunks split records
//! at arbitrary offsets, including mid-UTF-8, so nothing may assume a chunk is
//! a whole line.

/// Incremental SSE decoder. Feed it bytes; take whole `data:` payloads out.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes received but not yet forming a complete line.
    buf: Vec<u8>,
    /// `data:` lines collected for the record currently being built.
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a network chunk; returns every complete `data:` payload it closed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();

        // Consume whole lines only; a trailing partial line stays buffered.
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line = self.buf.drain(..=nl).collect::<Vec<u8>>();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // tolerate CRLF
            }

            // Invalid UTF-8 from a provider is a bug on their side; drop the
            // line rather than killing the stream.
            let Ok(line) = String::from_utf8(line) else {
                continue;
            };

            if line.is_empty() {
                // Blank line: the record is complete.
                if !self.data.is_empty() {
                    out.push(self.data.join("\n"));
                    self.data.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // comment / keep-alive
            }

            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                // A field with no colon is a name with an empty value.
                None => (line.as_str(), ""),
            };
            if field == "data" {
                self.data.push(value.to_string());
            }
        }
        out
    }

    /// Flush at end of stream. A well-behaved server terminates the last
    /// record with a blank line, but not all do; anything still pending is a
    /// complete record once the connection closes.
    pub fn finish(&mut self) -> Option<String> {
        if self.data.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.data).join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_one_record_per_blank_line() {
        let mut d = SseDecoder::new();
        let out = d.push(b"data: {\"a\":1}\n\ndata: {\"a\":2}\n\n");
        assert_eq!(out, vec!["{\"a\":1}", "{\"a\":2}"]);
    }

    #[test]
    fn tolerates_chunk_boundaries_anywhere() {
        // The same payload delivered one byte at a time must decode identically.
        let whole = b"data: {\"hello\":\"world\"}\n\ndata: [DONE]\n\n";
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        for b in whole {
            out.extend(d.push(&[*b]));
        }
        assert_eq!(out, vec!["{\"hello\":\"world\"}", "[DONE]"]);
    }

    #[test]
    fn splits_mid_utf8_without_corrupting() {
        // "né" — the 'é' is two bytes; split between them.
        let payload = "data: {\"t\":\"né\"}\n\n".as_bytes().to_vec();
        let cut = payload.iter().position(|&b| b == 0xC3).unwrap() + 1;
        let mut d = SseDecoder::new();
        let mut out = d.push(&payload[..cut]);
        out.extend(d.push(&payload[cut..]));
        assert_eq!(out, vec!["{\"t\":\"né\"}"]);
    }

    #[test]
    fn joins_multiple_data_lines_with_newline() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push(b"data: one\ndata: two\n\n"), vec!["one\ntwo"]);
    }

    #[test]
    fn skips_comments_and_other_fields() {
        let mut d = SseDecoder::new();
        let out = d.push(b": keep-alive\nevent: message\nid: 7\ndata: payload\n\n");
        assert_eq!(out, vec!["payload"]);
    }

    #[test]
    fn tolerates_crlf() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push(b"data: x\r\n\r\n"), vec!["x"]);
    }

    #[test]
    fn finish_flushes_an_unterminated_record() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: trailing\n").is_empty());
        assert_eq!(d.finish().as_deref(), Some("trailing"));
        assert_eq!(d.finish(), None, "finish must not repeat itself");
    }

    #[test]
    fn empty_data_line_is_still_a_record() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push(b"data:\n\n"), vec![""]);
    }
}
