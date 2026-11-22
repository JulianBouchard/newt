use std::time::Instant;

use super::Timed;
use crate::event::{Event, EventKind};
use crate::journal::{Journal, JournalEntry};
use crate::message::ToolCall;
use crate::sink::EventSink;
use crate::skills::Skill;
use crate::tools::{CallContext, ToolHost, ToolOutput, run_workspace_tool};
use crate::turn::TurnContext;

pub(super) async fn timed_read(context: &TurnContext<'_>, call: &ToolCall) -> (ToolOutput, u64) {
    let started = Instant::now();
    let output = run_workspace_tool(call, context.workspace).await;
    (output, elapsed_ms(started))
}

/// Runs one host call, passing the progress it reports to the sink.
pub(super) async fn timed_call<H: ToolHost>(
    host: &H,
    call: &ToolCall,
    call_context: &CallContext,
    context: &TurnContext<'_>,
    sink: &dyn EventSink,
) -> (ToolOutput, u64) {
    let started = Instant::now();
    let (sender, mut reports) = tokio::sync::mpsc::unbounded_channel();
    let call_context = CallContext {
        progress: Some(crate::tools::Progress(sender)),
        ..call_context.clone()
    };
    let report = |message: String| {
        sink.emit(Event::now(EventKind::ToolProgress {
            session: context.ids.session.to_owned(),
            turn: context.ids.turn.to_owned(),
            call_id: call.id.clone(),
            name: call.name.clone(),
            message,
        }));
    };
    let running = host.call(call, &call_context);
    tokio::pin!(running);
    let output = loop {
        tokio::select! {
            biased;
            Some(message) = reports.recv() => report(message),
            output = &mut running => break output,
        }
    };
    while let Ok(message) = reports.try_recv() {
        report(message);
    }
    (output, elapsed_ms(started))
}

pub(super) async fn timed_skill_load<H: ToolHost>(
    host: &H,
    call: &ToolCall,
    skill: &Skill,
) -> Timed {
    let started = Instant::now();
    let output = host
        .load_skill(skill, crate::skills::requested_file(call))
        .await;
    (output, elapsed_ms(started))
}

pub(super) fn call_context(context: &TurnContext<'_>, call: &ToolCall) -> CallContext {
    CallContext {
        idempotency_key: format!("{}:{}:{}", context.ids.session, context.ids.turn, call.id),
        skills_loaded: context.skills_loaded(),
        pages_read: context.workspace.pages_read(),
        traceparent: Some(crate::otlp::tool_traceparent(
            context.ids.session,
            context.ids.turn,
            context.ids.started,
            &call.id,
        )),
        progress: None,
    }
}

pub(super) fn record_output<J: Journal>(journal: &J, call: &ToolCall, output: &ToolOutput) {
    journal.append(JournalEntry::ToolOutput {
        call_id: call.id.clone(),
        output: output.clone(),
    });
}

pub(super) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
