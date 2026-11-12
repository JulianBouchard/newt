use newt_core::checkpoint::COMPACTION_INSTRUCTION;
use newt_core::event::{Event, EventKind};
use newt_core::journal::NoJournal;
use newt_core::prompt::Lane;
use newt_core::provider::Model;
use newt_core::sink::{EventSink, NoSink};
use newt_core::steering::NoSteering;
use newt_core::turn::{TurnDeps, TurnInput, TurnSettings, run_turn};

use crate::backend::{Backend, Prepared};
use crate::checkpoints::tail_cut;
use crate::runner::Inner;
use crate::sessions::{CommitError, Lease};

pub(crate) async fn compact_if_over_budget<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    prepared: &Prepared<B::Host>,
) -> anyhow::Result<()> {
    let state = inner.store.compaction_state(lease).await?;
    let budget = i64::from(prepared.history_budget);
    let tokens_before = state.tokens();
    if tokens_before <= budget {
        return Ok(());
    }
    let Some(through_seq) = tail_cut(&state.rows, budget / 4) else {
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
            turn: &turn_id,
            system: &prepared.system,
            external_tools: &prepared.tools,
            history: &history,
            context: Some(COMPACTION_INSTRUCTION.to_owned()),
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
            lane: Lane::Background,
            compaction: Some(state.open_commitments.as_slice()),
        },
    )
    .await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(failure) => {
            tracing::error!(error = %failure.error, agent = %lease.agent, key = %lease.key, "compaction failed; history stays as it is until the next try");
            return Ok(());
        }
    };
    let Some(checkpoint) = outcome.checkpoint else {
        anyhow::bail!("compaction ended without a checkpoint");
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
