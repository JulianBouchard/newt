use std::time::Duration;

use newt_core::checkpoint::COMPACTION_INSTRUCTION;
use newt_core::event::{Event, EventKind};
use newt_core::journal::NoJournal;
use newt_core::prompt::Lane;
use newt_core::provider::Model;
use newt_core::sink::{EventSink, NoSink};
use newt_core::steering::NoSteering;
use newt_core::turn::{Compaction, TurnDeps, TurnInput, TurnSettings, run_turn};

use crate::backend::{Backend, Prepared};
use crate::checkpoints::tail_cut;
use crate::runner::Inner;
use crate::sessions::{CommitError, Lease};

const COMPACTION_RETRY_BASE: Duration = Duration::from_mins(10);

pub(crate) async fn compact_if_over_budget<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    prepared: &Prepared<B::Host>,
) -> anyhow::Result<()> {
    let state = inner.store.compaction_state(lease).await?;
    let budget = i64::from(prepared.history_budget);
    let tokens_before = state.tokens();
    if tokens_before <= budget || !state.due {
        return Ok(());
    }
    let Some(through_seq) = tail_cut(&state.rows, budget / 4) else {
        tracing::warn!(agent = %lease.agent, key = %lease.key, tokens_before, budget, "history is over budget but holds no whole turn to compact");
        return Ok(());
    };
    let history = inner.store.history(lease).await?;
    let turn_id = uuid::Uuid::now_v7().to_string();
    let session = format!("{}/{}", lease.agent, lease.key);
    let cancel = inner.turns.child_token();
    let sink: &dyn EventSink = inner.instrumentation.as_deref().unwrap_or(&NoSink);
    tracing::info!(agent = %lease.agent, key = %lease.key, tokens_before, budget, "compacting session history");
    let result = run_turn(
        &TurnDeps {
            model: &inner.model,
            host: &prepared.host,
            steering: &NoSteering,
            journal: &NoJournal,
            sink,
            cancel: &cancel,
        },
        TurnInput {
            session: &session,
            cache_key: &inner.cache_keys.key(&lease.agent, &lease.key),
            turn: &turn_id,
            system: &prepared.system,
            external_tools: &prepared.tools,
            history: &history,
            context: Some(COMPACTION_INSTRUCTION.to_owned()),
            skills: &prepared.skills,
            events: Vec::new(),
            workspace: &prepared.workspace,
            replay: Vec::new(),
        },
        &TurnSettings {
            model: &prepared.model,
            first_call_effort: prepared.effort,
            effort: prepared.effort,
            max_output_tokens: prepared.max_output_tokens,
            max_model_calls: prepared.max_model_calls,
            max_turn_tokens: None,
            recite: false,
            require_end_turn: false,
            lane: Lane::Background,
            reset: false,
            compaction: Some(Compaction {
                open_commitments: &state.open_commitments,
                max_checkpoint_chars: usize::try_from(budget).unwrap_or(usize::MAX),
            }),
        },
    )
    .await;
    let (outcome, checkpoint) = match result {
        Ok(mut outcome) => match outcome.checkpoint.take() {
            Some(checkpoint) => (outcome, checkpoint),
            None => return compaction_failed(inner, lease, "it ended without a checkpoint").await,
        },
        Err(failure) => return compaction_failed(inner, lease, &failure.error.to_string()).await,
    };
    match inner
        .store
        .commit_compaction(
            lease,
            turn_id.clone(),
            outcome.committed,
            through_seq,
            checkpoint,
        )
        .await
    {
        Ok(()) => {}
        Err(CommitError::Fenced) => {
            tracing::warn!(agent = %lease.agent, key = %lease.key, "lease lost before the checkpoint was saved");
            return Ok(());
        }
        Err(CommitError::Database(error)) => return Err(error.into()),
    }
    let tokens_after = inner.store.compaction_state(lease).await?.tokens();
    tracing::info!(agent = %lease.agent, key = %lease.key, tokens_before, tokens_after, "checkpoint saved");
    sink.emit(Event::now(EventKind::CompactionCompleted {
        session,
        turn: turn_id,
        tokens_before,
        tokens_after,
    }));
    Ok(())
}

async fn compaction_failed<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    error: &str,
) -> anyhow::Result<()> {
    let failures = inner
        .store
        .record_compaction_failure(lease, COMPACTION_RETRY_BASE.as_secs_f64())
        .await?;
    tracing::error!(%error, failures, agent = %lease.agent, key = %lease.key, "compaction failed; history stays as it is and compaction waits before trying again");
    Ok(())
}
