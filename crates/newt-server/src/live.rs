//! Notifications that something changed for a session: its inbox got an
//! event, or its live feed got new rows. They come from Postgres `NOTIFY`
//! (other instances) and from this instance's own writers.
//!
//! Each subscriber wants one session, so [`Topics`] keeps one `watch` channel
//! per channel and topic: a notification wakes only that session's live feeds
//! and its running turn, not every subscriber on the instance. Only the
//! runner's wake loop needs every inbox topic, and gets them from
//! [`Topics::inbox`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::store::{CANCEL_CHANNEL, INBOX_CHANNEL, LIVE_CHANNEL};

const INBOX_BUFFER: usize = 4096;
const CANCEL_BUFFER: usize = 256;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Live,
    Inbox,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub channel: Channel,
    pub topic: String,
}

type Key = (Channel, String);

#[derive(Debug)]
struct Hub {
    inbox: broadcast::Sender<String>,
    cancel: broadcast::Sender<String>,
    watched: Mutex<HashMap<Key, watch::Sender<()>>>,
}

impl Hub {
    fn watched(&self) -> MutexGuard<'_, HashMap<Key, watch::Sender<()>>> {
        match self.watched.lock() {
            Ok(watched) => watched,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Where this instance's notifications go and who waits for them.
#[derive(Debug, Clone)]
pub struct Topics {
    hub: Arc<Hub>,
}

impl Default for Topics {
    fn default() -> Self {
        Self {
            hub: Arc::new(Hub {
                inbox: broadcast::channel(INBOX_BUFFER).0,
                cancel: broadcast::channel(CANCEL_BUFFER).0,
                watched: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl Topics {
    pub fn publish(&self, notification: Notification) {
        if let Some(sender) = self
            .hub
            .watched()
            .get(&(notification.channel, notification.topic.clone()))
        {
            sender.send_replace(());
        }
        match notification.channel {
            Channel::Inbox => {
                if self.hub.inbox.send(notification.topic).is_err() {
                    tracing::trace!("inbox notification with no wake loop");
                }
            }
            Channel::Cancel => {
                if self.hub.cancel.send(notification.topic).is_err() {
                    tracing::trace!("cancel notification with no cancel loop");
                }
            }
            Channel::Live => {}
        }
    }

    /// Wakes every watch, for when notifications may have been lost.
    pub fn wake_all(&self) {
        for sender in self.hub.watched().values() {
            sender.send_replace(());
        }
    }

    /// Every inbox topic notified from now on, for the runner's wake loop.
    pub fn inbox(&self) -> broadcast::Receiver<String> {
        self.hub.inbox.subscribe()
    }

    /// Every session a turn cancel was requested for from now on, for the
    /// runner's cancel loop.
    pub fn cancels(&self) -> broadcast::Receiver<String> {
        self.hub.cancel.subscribe()
    }

    /// Wakes on every notification for one topic published from now on.
    pub fn watch(&self, channel: Channel, topic: String) -> Watch {
        let key = (channel, topic);
        let receiver = self
            .hub
            .watched()
            .entry(key.clone())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe();
        Watch {
            hub: Arc::clone(&self.hub),
            key,
            receiver,
        }
    }
}

/// One subscriber to one topic. Notifications that arrive while nobody is
/// waiting collapse into one wake.
#[derive(Debug)]
pub struct Watch {
    hub: Arc<Hub>,
    key: Key,
    receiver: watch::Receiver<()>,
}

impl Watch {
    /// Whether a notification arrived since the watch was made or last
    /// asked, marking it seen.
    pub fn take_change(&mut self) -> bool {
        match self.receiver.has_changed() {
            Ok(changed) => {
                self.receiver.mark_unchanged();
                changed
            }
            Err(_closed) => true,
        }
    }

    pub async fn changed(&mut self) {
        if self.receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut watched = self.hub.watched();
        if watched
            .get(&self.key)
            .is_some_and(|sender| sender.receiver_count() <= 1)
        {
            watched.remove(&self.key);
        }
    }
}

pub async fn spawn_listener(pool: PgPool) -> Result<(Topics, JoinHandle<()>), sqlx::Error> {
    let topics = Topics::default();
    let publisher = topics.clone();
    let mut listener = subscribe(&pool).await?;
    let task = tokio::spawn(async move {
        loop {
            if let Err(error) = forward(&mut listener, &publisher).await {
                tracing::error!(%error, "live listener lost its connection, reconnecting");
                tokio::time::sleep(RECONNECT_DELAY).await;
                match subscribe(&pool).await {
                    Ok(fresh) => {
                        listener = fresh;
                        publisher.wake_all();
                    }
                    Err(error) => tracing::error!(%error, "live listener could not reconnect"),
                }
            }
        }
    });
    Ok((topics, task))
}

async fn subscribe(pool: &PgPool) -> Result<PgListener, sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener
        .listen_all([LIVE_CHANNEL, INBOX_CHANNEL, CANCEL_CHANNEL])
        .await?;
    Ok(listener)
}

async fn forward(listener: &mut PgListener, publisher: &Topics) -> Result<(), sqlx::Error> {
    loop {
        let notification = listener.recv().await?;
        let channel = match notification.channel() {
            INBOX_CHANNEL => Channel::Inbox,
            CANCEL_CHANNEL => Channel::Cancel,
            _ => Channel::Live,
        };
        publisher.publish(Notification {
            channel,
            topic: notification.payload().to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{Channel, Notification, Topics};

    fn notify(topics: &Topics, channel: Channel, topic: &str) {
        topics.publish(Notification {
            channel,
            topic: topic.to_owned(),
        });
    }

    #[tokio::test]
    async fn a_watch_wakes_only_for_its_own_topic() {
        let topics = Topics::default();
        let mut ana = topics.watch(Channel::Live, "a/ana".into());
        notify(&topics, Channel::Live, "a/bo");
        notify(&topics, Channel::Inbox, "a/ana");
        assert!(matches!(ana.receiver.has_changed(), Ok(false)));
        notify(&topics, Channel::Live, "a/ana");
        notify(&topics, Channel::Live, "a/ana");
        ana.changed().await;
        assert!(matches!(ana.receiver.has_changed(), Ok(false)));
    }

    #[tokio::test]
    async fn a_change_is_taken_once_and_wake_all_reaches_every_watch() {
        let topics = Topics::default();
        let mut ana = topics.watch(Channel::Inbox, "a/ana".into());
        let mut bo = topics.watch(Channel::Inbox, "a/bo".into());
        assert!(
            !ana.take_change(),
            "nothing arrived since the watch was made"
        );
        notify(&topics, Channel::Inbox, "a/ana");
        notify(&topics, Channel::Inbox, "a/ana");
        assert!(ana.take_change());
        assert!(
            !ana.take_change(),
            "two notifications collapse into one change"
        );
        topics.wake_all();
        assert!(ana.take_change());
        assert!(bo.take_change());
    }

    #[tokio::test]
    async fn the_last_watch_of_a_topic_removes_it() {
        let topics = Topics::default();
        let first = topics.watch(Channel::Inbox, "a/ana".into());
        let second = topics.watch(Channel::Inbox, "a/ana".into());
        drop(first);
        assert_eq!(topics.hub.watched().len(), 1);
        drop(second);
        assert!(topics.hub.watched().is_empty());
    }

    #[tokio::test]
    async fn the_cancel_loop_hears_every_cancel_topic_and_no_other() {
        let topics = Topics::default();
        let mut cancels = topics.cancels();
        notify(&topics, Channel::Inbox, "a/ana");
        notify(&topics, Channel::Cancel, "a/bo");
        assert_eq!(cancels.recv().await.ok().as_deref(), Some("a/bo"));
        assert!(cancels.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_wake_loop_hears_every_inbox_topic_and_no_live_one() {
        let topics = Topics::default();
        let mut inbox = topics.inbox();
        notify(&topics, Channel::Live, "a/ana");
        notify(&topics, Channel::Inbox, "a/bo");
        assert_eq!(inbox.recv().await.ok().as_deref(), Some("a/bo"));
        assert!(inbox.try_recv().is_err());
    }
}
