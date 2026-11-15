//! The reads and writes that start a turn. Each is one statement, so a turn
//! reaches its first model call in four round trips to Postgres: accept the
//! event, claim the session, read its work, start the turn with its history.

use std::time::Duration;

use newt_core::message::Message;
use serde_json::{Value, json};
use sqlx::types::Json;

use crate::journal_store::{JournalRow, OpenTurn, open_turn_from};
use crate::sessions::{Lease, Owned, PendingEvent};
use crate::store::{Store, timed_connection};

#[derive(Debug)]
pub enum NextTurn {
    /// A turn a crash interrupted, with the events it answers.
    Resume {
        open: OpenTurn,
        events: Vec<PendingEvent>,
        state: Option<Value>,
    },
    /// The events waiting for a new turn, never empty.
    Fresh {
        events: Vec<PendingEvent>,
        state: Option<Value>,
    },
    /// No open turn and no waiting events. `released` is false when the
    /// lease had already moved to another owner.
    Idle { released: bool },
}

/// A started turn's history, and the milliseconds spent waiting for a
/// connection.
pub type Begun = (Vec<Message>, u64);

type InboxRow = (i64, String, i16, String, Option<String>, bool, Value);

type NextRow = (
    bool,
    Json<Vec<JournalRow>>,
    Json<Vec<InboxRow>>,
    Option<Json<Value>>,
);

const CLAIM: &str = "update sessions set lease_owner = $3, lease_epoch = lease_epoch + 1, \
       lease_expires_at = now() + make_interval(secs => $4) \
     where agent = $1 and key = $2 \
       and (lease_owner is null or lease_expires_at < now()) \
       and (retry_after is null or retry_after <= now()) \
     returning lease_epoch";

const NEXT_OR_RELEASE: &str = "with released as ( \
       update sessions s set lease_owner = null, lease_expires_at = null \
       where s.agent = $1 and s.key = $2 and s.lease_owner = $3 and s.lease_epoch = $4 \
         and not exists (select 1 from inbox i \
           where i.agent = $1 and i.key = $2 and i.consumed_at is null) \
         and not exists (select 1 from turn_journal j where j.agent = $1 and j.key = $2) \
       returning 1) \
     select exists (select 1 from released), \
       coalesce((select jsonb_agg(jsonb_build_array(j.turn_id, j.seq, j.kind, j.payload) \
           order by j.turn_id, j.seq) \
         from turn_journal j where j.agent = $1 and j.key = $2), '[]'::jsonb), \
       coalesce((select jsonb_agg(jsonb_build_array(i.id, i.event_id, i.priority, i.policy, \
             i.payload->>'text', i.consumed_at is null, i.payload->'meta') order by i.id) \
         from inbox i where i.agent = $1 and i.key = $2 \
           and (i.consumed_at is null or i.id in ( \
             select (jsonb_array_elements_text(s.payload->'inbox_ids'))::bigint \
             from turn_journal s where s.agent = $1 and s.key = $2 and s.kind = 'start'))), \
         '[]'::jsonb), \
       (select provider_state from sessions where agent = $1 and key = $2)";

const BEGIN_TURN: &str = "with started as ( \
       insert into turn_journal (agent, key, turn_id, seq, kind, payload) \
       select $1, $2, $3, 0, 'start', $4 \
       where $7 and exists (select 1 from sessions \
         where agent = $1 and key = $2 and lease_owner = $5 and lease_epoch = $6 for share) \
       returning 1), \
     latest as ( \
       select through_seq, summary from checkpoints \
       where agent = $1 and key = $2 order by through_seq desc limit 1) \
     select (select count(*) from started), (select summary from latest), \
       coalesce((select jsonb_agg(h.entry order by h.seq) from history h \
         where h.agent = $1 and h.key = $2 and h.kind = 'message' \
           and h.seq > coalesce((select through_seq from latest), -1)), '[]'::jsonb)";

fn next_turn_from(
    journal: Vec<JournalRow>,
    inbox: Vec<InboxRow>,
    state: Option<Value>,
) -> Result<NextTurn, sqlx::Error> {
    let events = inbox.into_iter().map(
        |(inbox_id, event_id, priority, policy, text, pending, meta)| {
            let event = PendingEvent {
                inbox_id,
                event_id,
                interactive: priority == 0,
                steer: policy == "steer",
                text: text.unwrap_or_default(),
                meta: Some(meta).filter(|meta| !meta.is_null()),
            };
            (event, pending)
        },
    );
    if let Some(open) = open_turn_from(journal)? {
        let events = events
            .filter(|(event, _)| open.inbox_ids.contains(&event.inbox_id))
            .map(|(event, _)| event)
            .collect();
        return Ok(NextTurn::Resume {
            open,
            events,
            state,
        });
    }
    Ok(NextTurn::Fresh {
        events: events
            .filter(|(_, pending)| *pending)
            .map(|(event, _)| event)
            .collect(),
        state,
    })
}

fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

impl Store {
    /// Takes the session's lease. `None` when another owner holds it or the
    /// session is backing off. The work is read by a later statement, so its
    /// snapshot includes anything a previous owner committed while the claim
    /// waited for the row.
    pub fn claim(
        &self,
        agent: &str,
        key: &str,
        owner: &str,
        ttl: Duration,
    ) -> Owned<(Option<Lease>, u64)> {
        let pool = self.pool().clone();
        let (agent, key, owner) = (agent.to_owned(), key.to_owned(), owner.to_owned());
        Box::pin(async move {
            let (mut connection, waited) = timed_connection(&pool).await?;
            let epoch: Option<i64> = sqlx::query_scalar(CLAIM)
                .bind(&agent)
                .bind(&key)
                .bind(&owner)
                .bind(seconds(ttl))
                .fetch_optional(&mut *connection)
                .await?;
            let lease = epoch.map(|epoch| Lease {
                agent,
                key,
                owner,
                epoch,
            });
            Ok((lease, waited))
        })
    }

    /// What the session's next turn answers. When there is nothing, the same
    /// statement releases the lease.
    pub fn next_turn_or_release(&self, lease: &Lease) -> Owned<NextTurn> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let (released, Json(journal), Json(inbox), state): NextRow =
                sqlx::query_as(NEXT_OR_RELEASE)
                    .bind(&lease.agent)
                    .bind(&lease.key)
                    .bind(&lease.owner)
                    .bind(lease.epoch)
                    .fetch_one(&pool)
                    .await?;
            if released {
                return Ok(NextTurn::Idle { released: true });
            }
            match next_turn_from(journal, inbox, state.map(|Json(state)| state))? {
                NextTurn::Fresh { events, .. } if events.is_empty() => {
                    Ok(NextTurn::Idle { released: false })
                }
                next => Ok(next),
            }
        })
    }

    /// Writes a new turn's start and reads the history, in one statement.
    /// `None` when the lease moved before the start could be written.
    pub fn begin_turn(
        &self,
        lease: &Lease,
        turn_id: &str,
        inbox_ids: &[i64],
        fresh: bool,
    ) -> Owned<Option<Begun>> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        let turn_id = turn_id.to_owned();
        let start = Json(json!({ "inbox_ids": inbox_ids }));
        Box::pin(async move {
            let (mut connection, waited) = timed_connection(&pool).await?;
            let (started, summary, Json(messages)): (i64, Option<String>, Json<Vec<Message>>) =
                sqlx::query_as(BEGIN_TURN)
                    .bind(&lease.agent)
                    .bind(&lease.key)
                    .bind(&turn_id)
                    .bind(start)
                    .bind(&lease.owner)
                    .bind(lease.epoch)
                    .bind(fresh)
                    .fetch_one(&mut *connection)
                    .await?;
            if fresh && started == 0 {
                return Ok(None);
            }
            let mut history = Vec::with_capacity(messages.len() + 1);
            if let Some(summary) = summary {
                history.push(Message::user_text(summary));
            }
            history.extend(messages);
            Ok(Some((history, waited)))
        })
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::panic,
        reason = "tests fail loudly on setup errors"
    )]

    use super::*;

    fn start(inbox_ids: &[i64]) -> JournalRow {
        (
            "t1".into(),
            0,
            "start".into(),
            json!({ "inbox_ids": inbox_ids }),
        )
    }

    fn inbox(id: i64, pending: bool) -> InboxRow {
        (
            id,
            format!("e{id}"),
            0,
            "queue".into(),
            Some(format!("text {id}")),
            pending,
            json!({ "source": id }),
        )
    }

    fn ids(events: &[PendingEvent]) -> Vec<i64> {
        events.iter().map(|event| event.inbox_id).collect()
    }

    #[test]
    fn a_session_without_an_open_turn_answers_only_its_unconsumed_events() {
        let next = next_turn_from(
            Vec::new(),
            vec![inbox(3, true), inbox(5, true)],
            Some(json!({ "seen": 2 })),
        )
        .unwrap();
        let NextTurn::Fresh { events, state } = next else {
            panic!("expected a fresh turn, got {next:?}");
        };
        assert_eq!(ids(&events), vec![3, 5]);
        assert!(events[0].interactive, "priority 0 is interactive");
        assert_eq!(events[1].meta, Some(json!({ "source": 5 })));
        assert_eq!(
            state,
            Some(json!({ "seen": 2 })),
            "the provider's state rides along"
        );
    }

    #[test]
    fn an_open_turn_resumes_with_exactly_the_events_it_started_on() {
        let next = next_turn_from(
            vec![start(&[3, 4])],
            vec![inbox(3, false), inbox(4, true), inbox(9, true)],
            None,
        )
        .unwrap();
        let NextTurn::Resume { open, events, .. } = next else {
            panic!("expected a resumed turn, got {next:?}");
        };
        assert_eq!(open.turn_id, "t1");
        assert_eq!(ids(&events), vec![3, 4], "event 9 waits for the next turn");
    }
}
