#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use newt_core::event::{Event, EventKind};
use newt_core::journal::NoJournal;
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Lane, ModelRequest};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::EventSink;
use newt_core::steering::{Steered, Steering};
use newt_core::tools::NoTools;
use newt_core::turn::{TurnDeps, TurnInput, TurnOutcome, TurnSettings, run_turn};
use newt_core::workspace::Workspace;
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct SlowModel {
    script: Mutex<VecDeque<Message>>,
    delay: Duration,
    output_first: bool,
    calls: AtomicUsize,
    scratches: Mutex<Vec<Vec<Message>>>,
}

impl SlowModel {
    fn new(script: Vec<Message>, delay: Duration, output_first: bool) -> Self {
        Self {
            script: Mutex::new(script.into()),
            delay,
            output_first,
            calls: AtomicUsize::new(0),
            scratches: Mutex::new(Vec::new()),
        }
    }
}

impl Model for SlowModel {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.scratches
            .lock()
            .unwrap()
            .push(request.prompt.scratch.to_vec());
        if self.output_first {
            on_event(StreamEvent::OutputStarted);
        }
        tokio::time::sleep(self.delay).await;
        let message = self.script.lock().unwrap().pop_front();
        message
            .map(|message| ModelResponse {
                message,
                usage: Usage::default(),
                stop: StopReason::ToolUse,
                timing: CallTiming::default(),
                response_id: None,
                request_ids: BTreeMap::new(),
            })
            .ok_or_else(|| ProviderError::Protocol("script exhausted".into()))
    }
}

struct ScheduledSteering {
    arrives_after: Option<Duration>,
    available_from_take: usize,
    takes: AtomicUsize,
    arrived: AtomicBool,
    delivered: AtomicBool,
}

impl ScheduledSteering {
    fn message() -> Steered {
        Steered {
            id: "s1".into(),
            message: Message::user_text("Actually, make it Berlin."),
        }
    }
}

impl Steering for ScheduledSteering {
    fn take_new(&self) -> impl Future<Output = Vec<Steered>> + Send {
        let take = self.takes.fetch_add(1, Ordering::Relaxed);
        let ready = match self.arrives_after {
            Some(_) => self.arrived.load(Ordering::Relaxed),
            None => take >= self.available_from_take,
        };
        let deliver = ready && !self.delivered.swap(true, Ordering::Relaxed);
        std::future::ready(if deliver {
            vec![Self::message()]
        } else {
            Vec::new()
        })
    }

    async fn arrived(&self) {
        match self.arrives_after {
            Some(delay) if !self.arrived.load(Ordering::Relaxed) => {
                tokio::time::sleep(delay).await;
                self.arrived.store(true, Ordering::Relaxed);
            }
            _ => std::future::pending::<()>().await,
        }
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<Event>>);

impl EventSink for Events {
    fn emit(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

fn end_turn(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: id.into(),
            name: "end_turn".into(),
            arguments: json!({ "nothing_new": "chat" }),
        })],
    }
}

fn read_profile() -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: "r1".into(),
            name: "read_file".into(),
            arguments: json!({ "paths": ["profile.md"] }),
        })],
    }
}

async fn turn(model: &SlowModel, steering: &ScheduledSteering, events: &Events) -> TurnOutcome {
    run_turn(
        &TurnDeps {
            model,
            host: &NoTools,
            steering,
            journal: &NoJournal,
            sink: events,
            cancel: &CancellationToken::new(),
        },
        TurnInput {
            session: "s",
            cache_key: "s",
            turn: "t",
            system: "You are a helpful agent.",
            external_tools: &[],
            history: &[],
            context: None,
            skills: &[],
            events: vec![Message::user_text("Find me roles in Lisbon.")],
            workspace: &Workspace::default(),
            replay: Vec::new(),
        },
        &TurnSettings {
            model: "slow",
            first_call_effort: None,
            effort: None,
            max_output_tokens: 1024,
            max_model_calls: 6,
            max_turn_tokens: None,
            lane: Lane::Interactive,
            compaction: None,
        },
    )
    .await
    .unwrap()
}

fn interrupted(events: &Events) -> usize {
    events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event.kind, EventKind::ModelCallInterrupted { .. }))
        .count()
}

#[tokio::test]
async fn a_message_between_calls_joins_the_next_call() {
    let model = SlowModel::new(vec![read_profile(), end_turn("e1")], Duration::ZERO, false);
    let steering = ScheduledSteering {
        arrives_after: None,
        available_from_take: 1,
        takes: AtomicUsize::new(0),
        arrived: AtomicBool::new(false),
        delivered: AtomicBool::new(false),
    };
    let events = Events::default();

    let outcome = turn(&model, &steering, &events).await;

    assert_eq!(outcome.steered, vec!["s1"]);
    let scratches = model.scratches.lock().unwrap();
    assert_eq!(
        scratches[1].last(),
        Some(&Message::user_text("Actually, make it Berlin."))
    );
    assert!(
        outcome
            .committed
            .contains(&Message::user_text("Actually, make it Berlin."))
    );
    assert_eq!(interrupted(&events), 0);
}

#[tokio::test]
async fn a_message_before_any_output_interrupts_and_reissues_the_call() {
    let model = SlowModel::new(vec![end_turn("e1")], Duration::from_millis(300), false);
    let steering = ScheduledSteering {
        arrives_after: Some(Duration::from_millis(30)),
        available_from_take: usize::MAX,
        takes: AtomicUsize::new(0),
        arrived: AtomicBool::new(false),
        delivered: AtomicBool::new(false),
    };
    let events = Events::default();

    let outcome = turn(&model, &steering, &events).await;

    assert_eq!(interrupted(&events), 1);
    assert_eq!(model.calls.load(Ordering::Relaxed), 2);
    assert_eq!(outcome.steered, vec!["s1"]);
    let scratches = model.scratches.lock().unwrap();
    assert_eq!(scratches[0].len(), 1);
    assert_eq!(
        scratches[1].last(),
        Some(&Message::user_text("Actually, make it Berlin."))
    );
}

#[tokio::test]
async fn a_message_after_output_started_never_interrupts() {
    let model = SlowModel::new(vec![end_turn("e1")], Duration::from_millis(200), true);
    let steering = ScheduledSteering {
        arrives_after: Some(Duration::from_millis(30)),
        available_from_take: usize::MAX,
        takes: AtomicUsize::new(0),
        arrived: AtomicBool::new(false),
        delivered: AtomicBool::new(false),
    };
    let events = Events::default();

    let outcome = turn(&model, &steering, &events).await;

    assert_eq!(interrupted(&events), 0);
    assert_eq!(model.calls.load(Ordering::Relaxed), 1);
    assert_eq!(outcome.steered, Vec::<String>::new());
}
