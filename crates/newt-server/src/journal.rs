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
const JOURNAL_BATCH: usize = 128;
const BROKEN: &str = "an earlier journal entry was lost, so nothing more can be sent safely";

type Ack = oneshot::Sender<Result<(), String>>;

/// A waiting writer, and whether its entry must reach the database's disk
/// (`synchronous_commit` on) before it is confirmed.
type Waiter = (Ack, bool);

/// Writes journal entries in order, each call as one statement, and answers the
/// writers waiting on them. A flush (a waiter with no entry) waits only for the
/// entries queued before it, so it never waits for a write that came later.
struct Writer {
    store: Store,
    lease: Lease,
    turn_id: String,
    seq: i32,
    lease_lost: CancellationToken,
    broken: Arc<AtomicBool>,
}

impl Writer {
    async fn write(&mut self, entries: Vec<JournalEntry>, durable: bool, acks: Vec<Ack>) {
        let result = if self.broken.load(Ordering::Relaxed) {
            if !entries.is_empty() {
                tracing::warn!(
                    entries = entries.len(),
                    "journal entries not written: an earlier write failed"
                );
            }
            Err(BROKEN.to_owned())
        } else if entries.is_empty() {
            Ok(())
        } else {
            let count = entries.len();
            let written = self
                .store
                .append_journal_batch(&self.lease, &self.turn_id, self.seq, entries, durable)
                .await;
            self.seq += i32::try_from(count).unwrap_or(i32::MAX);
            let result = match written {
                Ok(true) => Ok(()),
                Ok(false) => {
                    self.lease_lost.cancel();
                    Err("the lease moved to another owner".to_owned())
                }
                Err(error) => Err(error.to_string()),
            };
            if let Err(error) = &result {
                self.broken.store(true, Ordering::Relaxed);
                tracing::warn!(%error, entries = count, "journal entries not written");
            }
            result
        };
        for ack in acks {
            if ack.send(result.clone()).is_err() {
                tracing::warn!("journal acknowledgement had no listener");
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnJournal {
    sender: mpsc::Sender<(Option<JournalEntry>, Option<Waiter>)>,
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
        let (sender, mut receiver) =
            mpsc::channel::<(Option<JournalEntry>, Option<Waiter>)>(JOURNAL_BUFFER);
        let broken = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&broken);
        let writer = tokio::spawn(async move {
            let mut writer = Writer {
                store,
                lease,
                turn_id,
                seq: next_seq,
                lease_lost,
                broken: flag,
            };
            let mut batch = Vec::with_capacity(JOURNAL_BATCH);
            while receiver.recv_many(&mut batch, JOURNAL_BATCH).await > 0 {
                let mut entries = Vec::new();
                let mut acks = Vec::new();
                let mut durable = false;
                for (entry, waiter) in batch.drain(..) {
                    let flush = entry.is_none();
                    entries.extend(entry);
                    if let Some((ack, synced)) = waiter {
                        durable |= synced;
                        acks.push(ack);
                    }
                    if flush {
                        writer
                            .write(
                                std::mem::take(&mut entries),
                                durable,
                                std::mem::take(&mut acks),
                            )
                            .await;
                        durable = false;
                    }
                }
                writer.write(entries, durable, acks).await;
            }
        });
        (Self { sender, broken }, writer)
    }
}

impl Journal for TurnJournal {
    fn append(&self, entry: JournalEntry) {
        if self.sender.try_send((Some(entry), None)).is_err() {
            self.broken.store(true, Ordering::Relaxed);
            tracing::warn!("journal buffer full or closed, entry dropped");
        }
    }

    fn append_and_wait(
        &self,
        entry: JournalEntry,
    ) -> impl Future<Output = Result<(), JournalError>> + Send {
        self.wait_for(entry, true)
    }

    fn flush_relaxed(&self) -> impl Future<Output = Result<(), JournalError>> + Send {
        let (ack, done) = oneshot::channel();
        let queued = !self.broken.load(Ordering::Relaxed)
            && self.sender.try_send((None, Some((ack, false)))).is_ok();
        if !queued {
            self.broken.store(true, Ordering::Relaxed);
        }
        async move {
            if !queued {
                return Err(JournalError(BROKEN.to_owned()));
            }
            done.await
                .map_err(|_| JournalError("journal writer stopped".to_owned()))?
                .map_err(JournalError)
        }
    }
}

impl TurnJournal {
    fn wait_for(
        &self,
        entry: JournalEntry,
        synced: bool,
    ) -> impl Future<Output = Result<(), JournalError>> + Send + use<> {
        let sender = self.sender.clone();
        let broken = Arc::clone(&self.broken);
        async move {
            if broken.load(Ordering::Relaxed) {
                return Err(JournalError(BROKEN.to_owned()));
            }
            let (ack, done) = oneshot::channel();
            sender
                .send((Some(entry), Some((ack, synced))))
                .await
                .map_err(|_| JournalError("journal writer stopped".to_owned()))?;
            done.await
                .map_err(|_| JournalError("journal writer stopped".to_owned()))?
                .map_err(JournalError)
        }
    }
}
