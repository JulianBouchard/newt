//! The reads and writes that start a turn, grouped so each group takes one
//! connection from the pool: a burst of sessions starting at once queues on
//! the pool three times per turn instead of five.

use newt_core::message::Message;

use crate::checkpoints::history_on;
use crate::journal_store::{OpenTurn, events_by_ids_on, open_turn_on, start_turn_on};
use crate::sessions::{Lease, Owned, PendingEvent, pending_events_on};
use crate::store::{Store, timed_connection};

#[derive(Debug)]
pub enum NextTurn {
    /// A turn a crash interrupted, with the events it answers.
    Resume {
        open: OpenTurn,
        events: Vec<PendingEvent>,
    },
    /// The events waiting for a new turn, empty when there are none.
    Fresh(Vec<PendingEvent>),
}

/// A started turn's history, and the milliseconds spent waiting for a
/// connection.
pub type Begun = (Vec<Message>, u64);

impl Store {
    /// What the session's next turn answers: an interrupted turn to resume,
    /// or the events waiting in the inbox.
    pub fn next_turn(&self, lease: &Lease) -> Owned<NextTurn> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let mut connection = pool.acquire().await?;
            if let Some(open) = open_turn_on(&mut connection, &lease).await? {
                let events = events_by_ids_on(&mut connection, &lease, &open.inbox_ids).await?;
                return Ok(NextTurn::Resume { open, events });
            }
            Ok(NextTurn::Fresh(
                pending_events_on(&mut connection, &lease).await?,
            ))
        })
    }

    /// Writes a new turn's start, then reads the history. `None` when the
    /// lease moved before the start could be written.
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
        let inbox_ids = inbox_ids.to_vec();
        Box::pin(async move {
            let (mut connection, waited) = timed_connection(&pool).await?;
            if fresh && !start_turn_on(&mut connection, &lease, &turn_id, &inbox_ids).await? {
                return Ok(None);
            }
            Ok(Some((history_on(&mut connection, &lease).await?, waited)))
        })
    }
}
