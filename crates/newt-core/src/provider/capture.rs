use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub(super) struct Capture {
    dir: PathBuf,
    sequence: Arc<AtomicU64>,
}

#[derive(Debug)]
pub(super) struct CaptureStream {
    file: File,
}

impl Capture {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) async fn begin(&self, body: &Value) -> std::io::Result<CaptureStream> {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        tokio::fs::create_dir_all(&self.dir).await?;
        let request = serde_json::to_vec_pretty(body).map_err(std::io::Error::other)?;
        tokio::fs::write(
            self.dir.join(format!("{sequence:04}-request.json")),
            request,
        )
        .await?;
        let file = File::create(self.dir.join(format!("{sequence:04}-response.sse"))).await?;
        Ok(CaptureStream { file })
    }
}

impl CaptureStream {
    pub(super) async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes).await?;
        self.file.flush().await
    }
}
