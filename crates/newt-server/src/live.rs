//! Notifications that something changed for a session: its inbox got an
//! event, or its live feed got new rows. They come from Postgres `NOTIFY`
//! (other instances) and from this instance's own writers.
//!
//! Each subscriber wants one session, so [`Topics`] keeps one `watch` channel
//! per channel and topic: a notification wakes only that session's live feeds
//! and its running turn, not every subscriber on the instance. Only the
//! runner's wake loop needs every inbox topic, and gets them from
//! [`Topics::inbox`].
//!
//! A watched session's live feed also keeps its newest frames in memory: the
//! instance that writes them hands them over with their sequence numbers, so
//! its own live feeds read the database only to resume, to fill a gap, or for
//! frames another instance wrote. A live notification carries the sequence
//! number of the newest frame, and one the instance already holds (its own
//! `NOTIFY` coming back) wakes nobody.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::store::{CANCEL_CHANNEL, INBOX_CHANNEL, LIVE_CHANNEL, LiveEvent};

const INBOX_BUFFER: usize = 4096;
const CANCEL_BUFFER: usize = 256;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Frames kept per watched session. A feed further behind reads the database.
const LIVE_TAIL: usize = 256;

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

/// A watched session's newest live frames.
#[derive(Debug, Default)]
struct Tail {
    frames: BTreeMap<i64, Value>,
    /// The newest sequence number this instance wrote or holds.
    newest: i64,
    /// Counts notifications of frames this instance does not hold.
    remote: u64,
}

#[derive(Debug)]
struct Hub {
    inbox: broadcast::Sender<String>,
    cancel: broadcast::Sender<String>,
    watched: Mutex<HashMap<Key, watch::Sender<()>>>,
    tails: Mutex<HashMap<String, Tail>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Hub {
    fn watched(&self) -> MutexGuard<'_, HashMap<Key, watch::Sender<()>>> {
        locked(&self.watched)
    }

    fn tails(&self) -> MutexGuard<'_, HashMap<String, Tail>> {
        locked(&self.tails)
    }
}

/// A live notification's topic and newest sequence number. A payload without
/// the number (from an older instance) has none.
fn live_payload(payload: &str) -> (&str, Option<i64>) {
    match payload.rsplit_once('/') {
        Some((topic, seq)) if topic.contains('/') => match seq.parse() {
            Ok(seq) => (topic, Some(seq)),
            Err(_) => (payload, None),
        },
        _ => (payload, None),
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
                tails: Mutex::new(HashMap::new()),
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

    /// Frames this instance just wrote to a session's live feed. A watched
    /// session keeps them for its feeds, which are then woken.
    pub fn append_live(&self, topic: &str, frames: Vec<LiveEvent>) {
        if let Some(tail) = self.hub.tails().get_mut(topic) {
            for frame in frames {
                tail.newest = tail.newest.max(frame.seq);
                tail.frames.insert(frame.seq, frame.event);
            }
            while tail.frames.len() > LIVE_TAIL {
                tail.frames.pop_first();
            }
        }
        self.publish(Notification {
            channel: Channel::Live,
            topic: topic.to_owned(),
        });
    }

    /// A live notification from Postgres. Frames this instance already holds
    /// wake nobody; any other makes the session's feeds read the database.
    fn remote_live(&self, payload: &str) {
        let (topic, seq) = live_payload(payload);
        if let Some(tail) = self.hub.tails().get_mut(topic) {
            if seq.is_some_and(|seq| seq <= tail.newest) {
                return;
            }
            tail.remote += 1;
        }
        self.publish(Notification {
            channel: Channel::Live,
            topic: topic.to_owned(),
        });
    }

    /// Wakes every watch, for when notifications may have been lost.
    pub fn wake_all(&self) {
        for tail in self.hub.tails().values_mut() {
            tail.remote += 1;
        }
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
        let mut watched = self.hub.watched();
        let receiver = watched
            .entry(key.clone())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe();
        if channel == Channel::Live {
            self.hub.tails().entry(key.1.clone()).or_default();
        }
        drop(watched);
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

    /// The live frames held in memory that follow `cursor` without a hole,
    /// and whether the feed must read the database: frames past a hole, or
    /// frames another instance wrote since `seen_remote`.
    pub fn live_after(&self, cursor: i64, seen_remote: &mut u64) -> (Vec<LiveEvent>, bool) {
        let tails = self.hub.tails();
        let Some(tail) = tails.get(&self.key.1) else {
            return (Vec::new(), true);
        };
        let remote = tail.remote != *seen_remote;
        *seen_remote = tail.remote;
        let Some(mut next) = cursor.checked_add(1) else {
            return (Vec::new(), remote);
        };
        let mut frames = Vec::new();
        for (&seq, event) in tail.frames.range(next..) {
            if seq != next {
                break;
            }
            frames.push(LiveEvent {
                seq,
                event: event.clone(),
            });
            match next.checked_add(1) {
                Some(following) => next = following,
                None => return (frames, remote),
            }
        }
        let hole = tail.frames.range(next..).next().is_some();
        (frames, remote || hole)
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
            if self.key.0 == Channel::Live {
                self.hub.tails().remove(&self.key.1);
            }
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
            _ => {
                publisher.remote_live(notification.payload());
                continue;
            }
        };
        publisher.publish(Notification {
            channel,
            topic: notification.payload().to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Channel, Notification, Topics, live_payload};
    use crate::store::LiveEvent;

    fn frames(seqs: &[i64]) -> Vec<LiveEvent> {
        seqs.iter()
            .map(|&seq| LiveEvent {
                seq,
                event: json!({ "seq": seq }),
            })
            .collect()
    }

    fn seqs(frames: &[LiveEvent]) -> Vec<i64> {
        frames.iter().map(|frame| frame.seq).collect()
    }

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

    #[test]
    fn a_live_payload_carries_its_newest_sequence_number_when_it_has_one() {
        assert_eq!(live_payload("a/ana/12"), ("a/ana", Some(12)));
        assert_eq!(live_payload("a/ana"), ("a/ana", None));
        assert_eq!(live_payload("a/123"), ("a/123", None));
    }

    #[tokio::test]
    async fn a_feed_takes_held_frames_in_order_and_reads_past_a_hole() {
        let topics = Topics::default();
        let ana = topics.watch(Channel::Live, "a/ana".into());
        let mut seen = 0;
        topics.append_live("a/ana", frames(&[1, 2, 3, 5]));
        let (held, must_read) = ana.live_after(0, &mut seen);
        assert_eq!(seqs(&held), [1, 2, 3]);
        assert!(must_read, "frame 4 is not in memory");
        let (held, must_read) = ana.live_after(3, &mut seen);
        assert_eq!(seqs(&held), [0_i64; 0]);
        assert!(must_read);
        let (held, must_read) = ana.live_after(5, &mut seen);
        assert_eq!(seqs(&held), [0_i64; 0]);
        assert!(!must_read, "nothing past the cursor");
    }

    #[tokio::test]
    async fn a_cursor_at_the_largest_sequence_number_takes_nothing() {
        let topics = Topics::default();
        let ana = topics.watch(Channel::Live, "a/ana".into());
        let mut seen = 0;
        topics.append_live("a/ana", frames(&[1, i64::MAX]));
        let (held, must_read) = ana.live_after(i64::MAX, &mut seen);
        assert_eq!(seqs(&held), [0_i64; 0]);
        assert!(!must_read);
        let (held, _) = ana.live_after(i64::MAX - 1, &mut seen);
        assert_eq!(seqs(&held), [i64::MAX]);
    }

    #[tokio::test]
    async fn an_instances_own_notification_wakes_nobody_and_another_reads() {
        let topics = Topics::default();
        let mut ana = topics.watch(Channel::Live, "a/ana".into());
        let mut seen = 0;
        topics.append_live("a/ana", frames(&[1, 2]));
        assert!(ana.take_change());
        topics.remote_live("a/ana/2");
        assert!(!ana.take_change(), "frames up to 2 are already held");
        assert!(!ana.live_after(2, &mut seen).1);
        topics.remote_live("a/ana/3");
        assert!(ana.take_change());
        assert!(
            ana.live_after(2, &mut seen).1,
            "frame 3 is another instance's"
        );
        topics.remote_live("a/ana");
        assert!(ana.take_change());
        assert!(ana.live_after(2, &mut seen).1, "no number: read to be sure");
    }

    #[tokio::test]
    async fn a_session_without_a_live_watch_keeps_no_frames() {
        let topics = Topics::default();
        topics.append_live("a/ana", frames(&[1]));
        assert!(topics.hub.tails().is_empty());
        let ana = topics.watch(Channel::Live, "a/ana".into());
        topics.append_live("a/ana", frames(&[2]));
        drop(ana);
        assert!(topics.hub.tails().is_empty());
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
