use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const FLUSH_EVERY: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
pub(super) struct TextCoalescer {
    pending: String,
    last_flush: Option<Instant>,
}

impl TextCoalescer {
    pub(super) fn push(&mut self, delta: &str, emit: &impl Fn(String)) {
        self.pending.push_str(delta);
        let first = self.last_flush.is_none();
        if first || ends_a_line_or_sentence(&self.pending) || self.due() {
            self.flush(emit);
        }
    }

    pub(super) fn flush(&mut self, emit: &impl Fn(String)) {
        self.last_flush = Some(Instant::now());
        if !self.pending.is_empty() {
            emit(std::mem::take(&mut self.pending));
        }
    }

    fn due(&self) -> bool {
        self.last_flush
            .is_some_and(|flushed| flushed.elapsed() >= FLUSH_EVERY)
    }
}

#[derive(Debug, Default)]
pub(super) struct ToolInputs {
    calls: BTreeMap<String, (String, TextCoalescer)>,
}

impl ToolInputs {
    pub(super) fn start(&mut self, id: String, name: String) {
        self.calls.insert(id, (name, TextCoalescer::default()));
    }

    pub(super) fn push(&mut self, id: &str, delta: &str, emit: &impl Fn(&str, &str, String)) {
        if let Some((name, pending)) = self.calls.get_mut(id) {
            pending.push(delta, &|text| emit(id, name, text));
        }
    }

    pub(super) fn flush(&mut self, id: &str, emit: &impl Fn(&str, &str, String)) {
        if let Some((name, pending)) = self.calls.get_mut(id) {
            pending.flush(&|text| emit(id, name, text));
        }
    }

    pub(super) fn flush_all(&mut self, emit: &impl Fn(&str, &str, String)) {
        for (id, (name, pending)) in &mut self.calls {
            pending.flush(&|text| emit(id, name, text));
        }
    }
}

fn ends_a_line_or_sentence(text: &str) -> bool {
    let trimmed = text.trim_end_matches(' ');
    text.ends_with('\n') || trimmed.len() < text.len() && trimmed.ends_with(['.', '!', '?'])
}

#[cfg(test)]
mod fuzz;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn newline_and_final_flush_deliver_all_text_in_order() {
        let sent = RefCell::new(Vec::new());
        let emit = |text: String| sent.borrow_mut().push(text);
        let mut text = TextCoalescer::default();
        for delta in ["He", "llo", " wor", "ld\n", "and", " more"] {
            text.push(delta, &emit);
        }
        text.flush(&emit);
        let sent = sent.into_inner();
        assert_eq!(sent.concat(), "Hello world\nand more");
        assert_eq!(sent, ["He", "llo world\n", "and more"]);
    }

    #[test]
    fn tool_inputs_are_kept_apart_per_call_and_flush_in_order() {
        let sent = RefCell::new(Vec::new());
        let emit = |id: &str, name: &str, text: String| {
            sent.borrow_mut()
                .push((id.to_owned(), name.to_owned(), text));
        };
        let mut inputs = ToolInputs::default();
        inputs.start("a".into(), "send".into());
        inputs.start("b".into(), "save".into());
        inputs.push("a", "{\"te", &emit);
        inputs.push("b", "{\"k", &emit);
        inputs.push("a", "xt\":\"hi\"}", &emit);
        inputs.push("unknown", "ignored", &emit);
        inputs.flush("a", &emit);
        inputs.flush_all(&emit);
        let sent = sent.into_inner();
        let of = |id: &str| -> String {
            sent.iter()
                .filter(|(call, _, _)| call == id)
                .map(|(_, _, text)| text.as_str())
                .collect()
        };
        assert_eq!(of("a"), "{\"text\":\"hi\"}");
        assert_eq!(of("b"), "{\"k");
        assert!(sent.iter().all(|(id, name, _)| matches!(
            (id.as_str(), name.as_str()),
            ("a", "send") | ("b", "save")
        )));
    }
}
