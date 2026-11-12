use std::future::Future;

use newt_core::journal::{Journal, JournalEntry, JournalError};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::sessions::Lease;
use crate::store::Store;

const JOURNAL_BUFFER: usize = 1024;

type Ack = oneshot::Sender<Result<(), String>>;

#[derive(Debug, Clone)]
pub struct TurnJournal {
    sender: mpsc::Sender<(JournalEntry, Option<Ack>)>,
}

impl TurnJournal {
    pub fn spawn(
        store: Store,
        lease: Lease,
        turn_id: String,
        next_seq: i32,
    ) -> (Self, JoinHandle<()>) {
        let (sender, mut receiver) = mpsc::channel::<(JournalEntry, Option<Ack>)>(JOURNAL_BUFFER);
        let writer = tokio::spawn(async move {
            let mut seq = next_seq;
            while let Some((entry, ack)) = receiver.recv().await {
                let result = match store.append_journal(&lease, &turn_id, seq, entry).await {
                    Ok(true) => Ok(()),
                    Ok(false) => Err("the lease moved to another owner".to_owned()),
                    Err(error) => Err(error.to_string()),
                };
                seq += 1;
                match ack {
                    Some(ack) => {
                        if ack.send(result).is_err() {
                            tracing::warn!("journal acknowledgement had no listener");
                        }
                    }
                    None => {
                        if let Err(error) = result {
                            tracing::warn!(%error, "journal entry not written");
                        }
                    }
                }
            }
        });
        (Self { sender }, writer)
    }
}

impl Journal for TurnJournal {
    fn append(&self, entry: JournalEntry) {
        if self.sender.try_send((entry, None)).is_err() {
            tracing::warn!("journal buffer full or closed, entry dropped");
        }
    }

    fn append_and_wait(
        &self,
        entry: JournalEntry,
    ) -> impl Future<Output = Result<(), JournalError>> + Send {
        let sender = self.sender.clone();
        async move {
            let (ack, done) = oneshot::channel();
            sender
                .send((entry, Some(ack)))
                .await
                .map_err(|_| JournalError("journal writer stopped".to_owned()))?;
            done.await
                .map_err(|_| JournalError("journal writer stopped".to_owned()))?
                .map_err(JournalError)
        }
    }
}
