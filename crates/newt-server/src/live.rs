use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::store::{INBOX_CHANNEL, LIVE_CHANNEL};

const TOPIC_BUFFER: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Live,
    Inbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub channel: Channel,
    pub topic: String,
}
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

pub fn spawn_listener(pool: PgPool) -> (broadcast::Sender<Notification>, JoinHandle<()>) {
    let (sender, _) = broadcast::channel(TOPIC_BUFFER);
    let publisher = sender.clone();
    let task = tokio::spawn(async move {
        loop {
            if let Err(error) = listen(&pool, &publisher).await {
                tracing::error!(%error, "live listener lost its connection, reconnecting");
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    });
    (sender, task)
}

async fn listen(
    pool: &PgPool,
    publisher: &broadcast::Sender<Notification>,
) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen_all([LIVE_CHANNEL, INBOX_CHANNEL]).await?;
    loop {
        let notification = listener.recv().await?;
        let channel = if notification.channel() == INBOX_CHANNEL {
            Channel::Inbox
        } else {
            Channel::Live
        };
        let message = Notification {
            channel,
            topic: notification.payload().to_owned(),
        };
        if publisher.send(message).is_err() {
            tracing::trace!("live notification with no subscribers");
        }
    }
}
