use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::store::LIVE_CHANNEL;

const TOPIC_BUFFER: usize = 4096;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

pub fn spawn_listener(pool: PgPool) -> (broadcast::Sender<String>, JoinHandle<()>) {
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

async fn listen(pool: &PgPool, publisher: &broadcast::Sender<String>) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(LIVE_CHANNEL).await?;
    loop {
        let notification = listener.recv().await?;
        if publisher.send(notification.payload().to_owned()).is_err() {
            tracing::trace!("live notification with no subscribers");
        }
    }
}
