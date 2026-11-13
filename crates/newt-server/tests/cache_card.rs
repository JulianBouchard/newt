#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::BTreeMap;

use newt_core::event::{Event, EventKind};
use newt_core::message::{Message, Part, Role};
use newt_core::prompt::{Prompt, ToolSpec};
use newt_core::provider::{CallTiming, StopReason, Usage};
use newt_server::cache_card::{CacheCard, Cause, Prices};
use serde_json::json;
use time::{Duration, OffsetDateTime};

fn reply(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::text(text)],
    }
}

const SYSTEM: &str = "You keep one person's tasks.";

fn tool(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({ "type": "object" }),
    }
}

struct Log {
    card: CacheCard,
    at: OffsetDateTime,
    turn: u32,
    key: String,
}

impl Log {
    fn new() -> Self {
        Self {
            card: CacheCard::new(300),
            at: OffsetDateTime::UNIX_EPOCH,
            turn: 0,
            key: "assistant/0".into(),
        }
    }

    fn emit(&mut self, kind: EventKind) {
        self.card.observe(&Event { at: self.at, kind });
    }

    /// One turn with one model call that read `cached` of 10,000 input tokens.
    fn turn(&mut self, tools: &[ToolSpec], history: &[Message], compaction: bool, cached: u64) {
        self.turn += 1;
        let turn = format!("t{}", self.turn);
        let prompt = Prompt {
            system: SYSTEM,
            tools,
            history,
            scratch: &[],
        };
        self.emit(EventKind::TurnStarted {
            session: "ana".into(),
            turn: turn.clone(),
            history_messages: history.len(),
            events: 1,
            compaction,
        });
        self.emit(EventKind::ModelCallStarted {
            session: "ana".into(),
            turn: turn.clone(),
            call: 0,
            model: "default".into(),
            cache_key: self.key.clone(),
            effort: None,
            prefix: prompt.prefix_fingerprint(),
            history: prompt.history_fingerprint(),
            scratch: prompt.scratch_fingerprint(),
            input: Vec::new(),
        });
        self.at += Duration::seconds(2);
        self.emit(EventKind::ModelCallFinished {
            session: "ana".into(),
            turn,
            call: 0,
            usage: Usage {
                input_tokens: 10_000,
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                output_tokens: 100,
                reasoning_tokens: 0,
            },
            timing: CallTiming::default(),
            stop: StopReason::EndTurn,
            response_id: None,
            request_ids: BTreeMap::new(),
            output: reply("ok"),
        });
        self.at += Duration::seconds(10);
    }

    fn causes(&self) -> Vec<Cause> {
        self.card
            .misses
            .iter()
            .map(|miss| miss.cause.clone())
            .collect()
    }
}

#[test]
fn a_changed_tool_description_is_named_with_where_it_sits_in_the_prefix() {
    let before = [
        tool("add_task", "Add a task."),
        tool("search", "Search tasks."),
    ];
    let after = [
        tool("add_task", "Add a task."),
        tool("search", "Search tasks by text."),
    ];
    let mut log = Log::new();
    log.turn(&before, &[], false, 9_000);
    log.turn(&after, &[], false, 100);
    let offset = Prompt {
        system: SYSTEM,
        tools: &before[..1],
        history: &[],
        scratch: &[],
    }
    .prefix_fingerprint()
    .parts
    .iter()
    .map(|part| part.bytes)
    .sum();
    assert_eq!(
        log.causes(),
        [Cause::PrefixChanged {
            part: "tool `search`".into(),
            offset
        }]
    );
}

#[test]
fn added_and_removed_tools_are_named() {
    let one = [tool("add_task", "Add a task.")];
    let two = [
        tool("add_task", "Add a task."),
        tool("search", "Search tasks."),
    ];
    let mut log = Log::new();
    log.turn(&one, &[], false, 9_000);
    log.turn(&two, &[], false, 0);
    log.turn(&one, &[], false, 0);
    let named: Vec<String> = log.causes().iter().map(ToString::to_string).collect();
    assert_eq!(named.len(), 2);
    assert!(
        named[0].starts_with("tool `search` (added) changed"),
        "{}",
        named[0]
    );
    assert!(
        named[1].starts_with("tool `search` (removed) changed"),
        "{}",
        named[1]
    );
}

#[test]
fn history_rewritten_by_compaction_is_told_apart_from_a_routing_miss() {
    let tools = [tool("add_task", "Add a task.")];
    let first = [Message::user_text("hi"), reply("hello")];
    let grown = [
        Message::user_text("hi"),
        reply("hello"),
        Message::user_text("add milk"),
    ];
    let compacted = [Message::user_text("<checkpoint>said hello</checkpoint>")];
    let mut log = Log::new();
    log.turn(&tools, &first, false, 0);
    log.turn(&tools, &grown, false, 0);
    log.turn(&tools, &compacted, true, 0);
    assert_eq!(
        log.causes(),
        [
            Cause::FirstCall,
            Cause::Unexplained { seconds: 10 },
            Cause::HistoryRewritten {
                at: 0,
                of: 3,
                compaction: true
            },
        ]
    );
}

#[test]
fn a_session_moved_to_another_cache_key_is_named_not_called_routing() {
    let tools = [tool("add_task", "Add a task.")];
    let mut log = Log::new();
    log.turn(&tools, &[], false, 9_500);
    log.key = "assistant/7".into();
    log.turn(&tools, &[], false, 0);
    assert_eq!(
        log.causes(),
        [Cause::KeyChanged {
            from: "assistant/0".into(),
            to: "assistant/7".into()
        }]
    );
}

#[test]
fn a_gap_longer_than_the_cache_lives_is_idle_and_hits_are_not_misses() {
    let tools = [tool("add_task", "Add a task.")];
    let mut log = Log::new();
    log.turn(&tools, &[], false, 9_500);
    log.at += Duration::minutes(10);
    log.turn(&tools, &[], false, 0);
    log.turn(&tools, &[], false, 9_500);
    assert_eq!(log.causes(), [Cause::Idle { seconds: 610 }]);
}

#[test]
fn the_card_totals_tokens_and_prices_them() {
    let tools = [tool("add_task", "Add a task.")];
    let mut log = Log::new();
    log.turn(&tools, &[], false, 0);
    log.turn(&tools, &[], false, 10_000);
    let prices: Prices = "2.5, 0.25, 10".parse().unwrap();
    let card = log.card.render(Some(prices), true);
    assert!(
        card.contains(
            "calls 2, input 20000 tokens, cached 50.0%, uncached 10000 tokens, cost $0.0295"
        ),
        "{card}"
    );
    assert!(card.contains("misses: 1"), "{card}");
    assert!("1,2".parse::<Prices>().is_err());
}
