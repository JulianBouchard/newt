const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SseError {
    #[error("server-sent event stream was not valid UTF-8")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("server-sent event line exceeded {MAX_PENDING_BYTES} bytes")]
    LineTooLong,
}

#[derive(Debug, Default)]
pub struct SseParser {
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=newline).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8(line)?;
            if let Some(event) = self.apply_line(&line) {
                events.push(event);
            }
        }
        if self.pending.len() > MAX_PENDING_BYTES {
            return Err(SseError::LineTooLong);
        }
        Ok(events)
    }

    fn apply_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            if self.data.is_empty() {
                self.event = None;
                return None;
            }
            return Some(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data).join("\n"),
            });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    fn event(name: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: name.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn parses_named_events_and_multi_line_data() {
        let mut parser = SseParser::default();
        let events = parser
            .feed(b"event: ping\ndata: {}\n\ndata: a\ndata: b\n\n")
            .unwrap();
        assert_eq!(events, vec![event(Some("ping"), "{}"), event(None, "a\nb")]);
    }

    #[test]
    fn events_split_across_chunks_are_reassembled() {
        let mut parser = SseParser::default();
        assert_eq!(parser.feed(b"event: mess").unwrap(), Vec::new());
        assert_eq!(parser.feed(b"age\r\ndata: {\"x\"").unwrap(), Vec::new());
        let events = parser.feed(b":1}\r\n\r\n").unwrap();
        assert_eq!(events, vec![event(Some("message"), "{\"x\":1}")]);
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let mut parser = SseParser::default();
        let events = parser
            .feed(b": keep-alive\nid: 7\nretry: 10\ndata: x\n\n")
            .unwrap();
        assert_eq!(events, vec![event(None, "x")]);
    }

    #[test]
    fn a_multi_byte_character_split_across_chunks_survives() {
        let mut parser = SseParser::default();
        let bytes = "data: café\n\n".as_bytes();
        let split = bytes.len() - 4;
        assert_eq!(parser.feed(&bytes[..split]).unwrap(), Vec::new());
        let events = parser.feed(&bytes[split..]).unwrap();
        assert_eq!(events, vec![event(None, "café")]);
    }

    #[test]
    fn invalid_utf8_and_endless_lines_are_errors() {
        let mut parser = SseParser::default();
        assert!(matches!(
            parser.feed(b"data: \xff\n\n"),
            Err(SseError::InvalidUtf8(_))
        ));
        let mut parser = SseParser::default();
        let huge = vec![b'a'; MAX_PENDING_BYTES + 1];
        assert!(matches!(parser.feed(&huge), Err(SseError::LineTooLong)));
    }
}
