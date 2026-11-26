use super::call::{next_response, take_arrivals};
use super::state::TurnState;
use super::{CallReport, TurnContext, TurnDeps, TurnError, TurnSettings};
use crate::checkpoint::Checkpoint;
use crate::event::{Event, EventKind};
use crate::journal::Journal;
use crate::message::{Message, Part, Role, ToolCall};
use crate::prompt::{ModelRequest, Prompt};
use crate::provider::Model;
use crate::steering::Steering;
use crate::tools::{EndTurn, ToolHost};

pub(super) struct Finish {
    pub(super) end: EndTurn,
    pub(super) reply: Option<String>,
    pub(super) checkpoint: Option<Checkpoint>,
}

fn remind_to_end(state: &mut TurnState, context: &TurnContext<'_>) {
    state.harness_notes.insert(state.scratch.len());
    state
        .scratch
        .push(Message::user_text(context.ending.reminder()));
}

fn finish_on_reply(context: &TurnContext<'_>, text: Option<&str>) -> Option<Finish> {
    let reply = text.filter(|_| context.ending.reply_ends_turn())?;
    Some(Finish {
        end: EndTurn {
            recorded: Vec::new(),
            nothing_new: Some("replied".to_owned()),
        },
        reply: Some(reply.to_owned()),
        checkpoint: None,
    })
}

fn text_of(message: &Message) -> Option<String> {
    let text = message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

pub(super) async fn drive<M: Model, H: ToolHost, S: Steering, J: Journal>(
    deps: &TurnDeps<'_, M, H, S, J>,
    context: &TurnContext<'_>,
    settings: &TurnSettings<'_>,
    state: &mut TurnState,
) -> Result<Finish, TurnError> {
    let mut reminded = false;
    let mut unended_text: Option<String> = None;
    for call in 0..settings.max_model_calls {
        state.before_call(settings, call)?;
        let arrived = take_arrivals(deps, state).await;
        if !arrived.is_empty() {
            deps.sink.emit(Event::now(EventKind::TurnSteered {
                session: context.ids.session.to_owned(),
                turn: context.ids.turn.to_owned(),
                events: arrived.len(),
            }));
            for steered in arrived {
                state.scratch.push(steered.message);
                state.steered.push(steered.id);
            }
        }
        let request = ModelRequest {
            model: settings.model,
            prompt: Prompt {
                system: context.system,
                tools: context.specs,
                history: context.history,
                scratch: &state.scratch,
            },
            effort: if call == 0 {
                settings.first_call_effort
            } else {
                settings.effort
            },
            max_output_tokens: settings.max_output_tokens,
            cache_key: context.ids.cache_key,
            lane: settings.lane,
        };
        let next = next_response(
            deps,
            &request,
            call,
            context,
            &mut state.replay,
            state.traced,
        )
        .await?;
        state.traced = state.scratch.len();
        let Some((response, mut early)) = next else {
            continue;
        };
        state.calls.push(CallReport {
            usage: response.usage,
            timing: response.timing,
            stop: response.stop,
        });

        let tool_calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
        let text = text_of(&response.message);
        if !response.message.parts.is_empty() {
            state.scratch.push(response.message);
        }
        if tool_calls.is_empty() {
            if let Some(finish) = finish_on_reply(context, text.as_deref()) {
                return Ok(finish);
            }
            if reminded {
                return Err(TurnError::NoEndTurn);
            }
            reminded = true;
            unended_text = text;
            remind_to_end(state, context);
            continue;
        }

        let batch =
            crate::batch::run_tools(&tool_calls, context, deps, &state.replay, &mut early).await;
        state.summaries.extend(batch.summaries);
        state.kept.extend(batch.kept);
        state.effects |= batch.effects;
        state.scratch.push(Message {
            role: Role::User,
            parts: batch.results.into_iter().map(Part::ToolResult).collect(),
        });
        if let Some(checkpoint) = batch.checkpoint {
            return Ok(Finish {
                end: EndTurn {
                    recorded: Vec::new(),
                    nothing_new: Some("checkpoint written".to_owned()),
                },
                reply: None,
                checkpoint: Some(checkpoint),
            });
        }
        if let Some(end) = batch.end {
            return Ok(Finish {
                end,
                reply: text.or(unended_text),
                checkpoint: None,
            });
        }
        unended_text = None;
    }
    Err(TurnError::TooManyModelCalls(settings.max_model_calls))
}
