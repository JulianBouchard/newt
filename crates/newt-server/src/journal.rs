use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use newt_core::journal::{Journal, JournalEntry, JournalError};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::sessions::Lease;
use crate::store::Store;

const JOURNAL_BUFFER: usize = 1024;
const BROKEN: &str = "an earlier journal entry was lost, so nothing more can be sent safely";

type Ack = oneshot::Sender<Result<(), String>>;

#[derive(Debug, Clone)]
pub struct TurnJournal {
    sender: mpsc::Sender<(JournalEntry, Option<Ack>)>,
    broken: Arc<AtomicBool>,
}

impl TurnJournal {
    pub fn spawn(
        store: Store,
        lease: Lease,
        turn_id: String,
        next_seq: i32,
        lease_lost: CancellationToken,
    ) -> (Self, JoinHandle<()>) {
        let (sender, mut receiver) = mpsc::channel::<(JournalEntry, Option<Ack>)>(JOURNAL_BUFFER);
        let broken = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&broken);
        let writer = tokio::spawn(async move {
            let mut seq = next_seq;
            while let Some((entry, ack)) = receiver.recv().await {
                let result = match store.append_journal(&lease, &turn_id, seq, entry).await {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        lease_lost.cancel();
                        Err("the lease moved to another owner".to_owned())
                    }
                    Err(error) => Err(error.to_string()),
                };
                seq += 1;
                if let Err(error) = &result {
                    flag.store(true, Ordering::Relaxed);
                    tracing::warn!(%error, "journal entry not written");
                }
                if let Some(ack) = ack
                    && ack.send(result).is_err()
                {
                    tracing::warn!("journal acknowledgement had no listener");
                }
            }
        });
        (Self { sender, broken }, writer)
    }
}

impl Journal for TurnJournal {
    fn append(&self, entry: JournalEntry) {
        if self.sender.try_send((entry, None)).is_err() {
            self.broken.store(true, Ordering::Relaxed);
            tracing::warn!("journal buffer full or closed, entry dropped");
        }
    }

    fn append_and_wait(
        &self,
        entry: JournalEntry,
    ) -> impl Future<Output = Result<(), JournalError>> + Send {
        let sender = self.sender.clone();
        let broken = Arc::clone(&self.broken);
        async move {
            if broken.load(Ordering::Relaxed) {
                return Err(JournalError(BROKEN.to_owned()));
            }
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
