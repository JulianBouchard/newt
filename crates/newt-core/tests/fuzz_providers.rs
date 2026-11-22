#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail loudly on setup errors"
)]

use std::convert::Infallible;
use std::sync::OnceLock;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::routing::post;
use futures_util::stream;
use newt_core::message::{Message, Part, ToolCall};
use newt_core::prompt::{Lane, ModelRequest, Prompt, ToolSpec};
use newt_core::provider::{
    Model, ModelResponse, Protocol, ProviderClient, ProviderError, StopReason, StreamEvent, Usage,
};
use proptest::prelude::*;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

#[path = "fuzz_providers/mutate.rs"]
mod mutate;
#[path = "fuzz_providers/quirks.rs"]
mod quirks;

#[derive(Clone, Copy)]
struct Fixture {
    protocol: Protocol,
    path: &'static str,
    body: &'static str,
}

impl std::fmt::Debug for Fixture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?} {} ({} bytes)",
            self.protocol,
            self.path,
            self.body.len()
        )
    }
}

const FIXTURES: [Fixture; 5] = [
    Fixture {
        protocol: Protocol::AnthropicMessages,
        path: "/v1/messages",
        body: include_str!("fixtures/anthropic_tool_use.sse"),
    },
    Fixture {
        protocol: Protocol::OpenaiResponses,
        path: "/responses",
        body: include_str!("fixtures/openai_text.sse"),
    },
    Fixture {
        protocol: Protocol::OpenaiResponses,
        path: "/responses",
        body: include_str!("fixtures/openai_tool_use.sse"),
    },
    Fixture {
        protocol: Protocol::OpenaiResponses,
        path: "/responses",
        body: include_str!("fixtures/openai_tool_use_streamed.sse"),
    },
    Fixture {
        protocol: Protocol::OpenaiChat,
        path: "/chat/completions",
        body: include_str!("fixtures/openai_chat_tool_use.sse"),
    },
];

const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct Outcome {
    events: Vec<StreamEvent>,
    result: Result<Summary, String>,
}

#[derive(Debug, PartialEq)]
struct Summary {
    parts: Vec<Part>,
    usage: Usage,
    stop: StopReason,
    response_id: Option<String>,
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| Runtime::new().expect("runtime"))
}

fn summarize(response: ModelResponse) -> Summary {
    Summary {
        parts: response.message.parts,
        usage: response.usage,
        stop: response.stop,
        response_id: response.response_id,
    }
}

fn run_chunks(fixture: Fixture, chunks: Vec<Vec<u8>>) -> Outcome {
    runtime().block_on(async {
        let paced = chunks.len() > 1;
        let app = Router::new().route(
            fixture.path,
            post(move |_: Bytes| {
                let chunks = chunks.clone();
                async move {
                    let body = stream::unfold(chunks.into_iter(), move |mut rest| async move {
                        let next = rest.next()?;
                        if paced {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        Some((Ok::<_, Infallible>(Bytes::from(next)), rest))
                    });
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(body))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = ProviderClient::new(fixture.protocol, base, "key").unwrap();
        let tools = [ToolSpec {
            name: "read_file".into(),
            description: "Read.".into(),
            input_schema: json!({ "type": "object" }),
        }];
        let history = [Message::user_text("Hi")];
        let request = ModelRequest {
            model: "test-model",
            prompt: Prompt {
                system: "system",
                tools: &tools,
                history: &history,
                scratch: &[],
            },
            effort: None,
            max_output_tokens: 1024,
            cache_key: "key",
            lane: Lane::Interactive,
        };
        let mut events = Vec::new();
        let cancel = CancellationToken::new();
        let mut collect = |event| events.push(event);
        let call = client.stream(&request, &cancel, &mut collect);
        let result = tokio::time::timeout(DEADLINE, call)
            .await
            .unwrap_or_else(|_| panic!("stream hung for {DEADLINE:?}"));
        server.abort();
        Outcome {
            events,
            result: result
                .map(summarize)
                .map_err(|error: ProviderError| error.to_string()),
        }
    })
}

fn single_chunk(text: &str) -> Vec<Vec<u8>> {
    vec![text.as_bytes().to_vec()]
}

fn split_at(bytes: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
    let mut points: Vec<usize> = cuts.iter().map(|cut| cut % (bytes.len() + 1)).collect();
    points.sort_unstable();
    points.dedup();
    let mut chunks = Vec::new();
    let mut start = 0;
    for point in points {
        chunks.push(bytes[start..point].to_vec());
        start = point;
    }
    chunks.push(bytes[start..].to_vec());
    chunks
}

fn variant(fixture: Fixture, crlf: bool, unicode: bool) -> String {
    let mut text = fixture.body.to_owned();
    if unicode {
        text = text
            .replace("Checking", "Ch\u{e9}ck\u{1f642}ing")
            .replace("your profile.", "y\u{fc}r pr\u{f6}file \u{65e5}\u{672c}.");
    }
    if crlf {
        text = text.replace('\n', "\r\n");
    }
    text
}

fn fixture_strategy() -> impl Strategy<Value = Fixture> {
    prop::sample::select(FIXTURES.to_vec())
}

fn ready_calls(events: &[StreamEvent]) -> Vec<&ToolCall> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallReady(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn check_tool_stream(outcome: &Outcome) -> Result<(), String> {
    let Ok(summary) = &outcome.result else {
        return Ok(());
    };
    let response_calls: Vec<&ToolCall> = summary
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    let ready = ready_calls(&outcome.events);
    if ready.len() != response_calls.len() {
        return Err(format!(
            "{} ready vs {} in response",
            ready.len(),
            response_calls.len()
        ));
    }
    for call in &response_calls {
        let matching = ready
            .iter()
            .filter(|candidate| candidate.id == call.id)
            .count();
        if matching != 1 {
            return Err(format!("{matching} ready events for {}", call.id));
        }
        if !ready.contains(call) {
            return Err(format!(
                "ready event for {} differs from the response",
                call.id
            ));
        }
        let started = outcome.events.iter().position(
            |event| matches!(event, StreamEvent::ToolCallStarted { id, .. } if *id == call.id),
        );
        let readied = outcome.events.iter().position(
            |event| matches!(event, StreamEvent::ToolCallReady(ready) if ready.id == call.id),
        );
        if started.is_none() || started > readied {
            return Err(format!("{} was not started before it was ready", call.id));
        }
        let streamed: String = outcome
            .events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolInputDelta { id, delta } if *id == call.id => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        let parsed: Value = if streamed.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&streamed)
                .map_err(|error| format!("deltas of {} are not JSON: {error}", call.id))?
        };
        if parsed != call.arguments {
            return Err(format!(
                "deltas of {} give {parsed}, the call has {}",
                call.id, call.arguments
            ));
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn rechunking_never_changes_events_or_response(
        fixture in fixture_strategy(),
        cuts in prop::collection::vec(any::<usize>(), 0..8),
        crlf in any::<bool>(),
        unicode in any::<bool>(),
    ) {
        let text = variant(fixture, crlf, unicode);
        let whole = run_chunks(fixture, single_chunk(&text));
        prop_assert!(whole.result.is_ok(), "{:?}", whole.result);
        let split = run_chunks(fixture, split_at(text.as_bytes(), &cuts));
        prop_assert_eq!(&split.events, &whole.events);
        prop_assert_eq!(&split.result, &whole.result);
    }

    #[test]
    fn tool_streams_are_consistent_at_any_chunking(
        fixture in fixture_strategy(),
        cuts in prop::collection::vec(any::<usize>(), 0..8),
    ) {
        let outcome = run_chunks(fixture, split_at(fixture.body.as_bytes(), &cuts));
        prop_assert!(outcome.result.is_ok());
        prop_assert!(check_tool_stream(&outcome).is_ok(), "{:?}", check_tool_stream(&outcome));
    }

    #[test]
    fn a_truncated_stream_is_an_error_or_the_full_response(
        fixture in fixture_strategy(),
        keep in any::<prop::sample::Index>(),
    ) {
        let whole = run_chunks(fixture, single_chunk(fixture.body));
        let end = keep.index(fixture.body.len());
        let outcome = run_chunks(fixture, single_chunk(&fixture.body[..end]));
        if let Ok(summary) = &outcome.result {
            prop_assert_eq!(Ok(summary), whole.result.as_ref());
        }
        prop_assert!(check_tool_stream(&outcome).is_ok());
    }

    #[test]
    fn arbitrary_bytes_end_in_an_error_without_panicking(
        fixture in fixture_strategy(),
        bytes in prop::collection::vec(any::<u8>(), 0..2048),
    ) {
        let outcome = run_chunks(fixture, vec![bytes]);
        prop_assert!(outcome.result.is_err());
    }

    #[test]
    fn json_with_the_wrong_shape_never_panics_or_hangs(
        fixture in fixture_strategy(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<prop::sample::Index>(), 0u8..7), 1..4),
    ) {
        let text = mutate::mutate(fixture.body, &edits);
        let outcome = run_chunks(fixture, single_chunk(&text));
        prop_assert!(outcome.events.len() < 10_000, "{text}");
    }
}

#[test]
fn a_malformed_json_event_is_an_error() {
    for fixture in FIXTURES {
        let lines: Vec<&str> = fixture.body.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let Some(json) = line.strip_prefix("data: {") else {
                continue;
            };
            let broken = format!("data: {{{}", &json[..json.len() / 2]);
            let mut edited = lines.clone();
            edited[index] = &broken;
            let outcome = run_chunks(fixture, single_chunk(&format!("{}\n", edited.join("\n"))));
            assert!(
                outcome.result.is_err(),
                "line {index} of {}: {broken}",
                fixture.path
            );
        }
    }
}
