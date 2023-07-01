use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures_util::StreamExt;
use tokio::fs::File;
use std::io::Write;
use tokio::fs::OpenOptions;
use std::task::{Poll, Context};
use std::pin::Pin;
use std::time::{UNIX_EPOCH, SystemTime};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("now is later than epoch")
        .as_millis() as u64
}

pub struct ArtifactStream {
    sender: hyper::body::Sender,
}

impl ArtifactStream {
    pub fn new(sender: hyper::body::Sender) -> Self {
        Self { sender }
    }
}

impl tokio::io::AsyncWrite for ArtifactStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8]
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut().sender.try_send_data(buf.to_vec().into()) {
            Ok(()) => {
                Poll::Ready(Ok(buf.len()))
            },
            _ => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}


pub struct ArtifactDescriptor {
    job_id: u64,
    pub artifact_id: u64,
    file: File,
}

impl ArtifactDescriptor {
    pub async fn new(job_id: u64, artifact_id: u64) -> Result<Self, String> {
        // TODO: jobs should be a configurable path
        let path = format!("artifacts/{}/{}", job_id, artifact_id);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|e| format!("couldn't open artifact file {}: {}", path, e))?;

        Ok(ArtifactDescriptor {
            job_id,
            artifact_id,
            file,
        })
    }

    pub async fn store_all(&mut self, mut data: axum::extract::BodyStream) -> Result<(), String> {
        loop {
            let chunk = data.next().await;

            let chunk = match chunk {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => {
                    eprintln!("error: {:?}", e);
                    return Err(format!("error reading: {:?}", e));
                }
                None => {
                    eprintln!("body done?");
                    return Ok(());
                }
            };

            let chunk = chunk.as_ref();

            self.file.write_all(chunk).await
                .map_err(|e| format!("failed to write: {:?}", e))?;
        }
    }
}

pub async fn forward_data(source: &mut (impl AsyncRead + Unpin), dest: &mut (impl AsyncWrite + Unpin)) -> Result<(), String> {
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n_read = source.read(&mut buf).await
            .map_err(|e| format!("failed to read: {:?}", e))?;

        if n_read == 0 {
            eprintln!("done reading!");
            return Ok(());
        }

        dest.write_all(&buf[..n_read]).await
            .map_err(|e| format!("failed to write: {:?}", e))?;
    }
}
/*
pub async fn forward_data(source: &mut (impl AsyncRead + Unpin), dest: &mut ArtifactStream) -> Result<(), String> {
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n_read = source.read(&mut buf).await
            .map_err(|e| format!("failed to read: {:?}", e))?;

        if n_read == 0 {
            eprintln!("done reading!");
            return Ok(());
        }

        dest.sender.send_data(buf[..n_read].to_vec().into()).await
            .map_err(|e| format!("failed to write: {:?}", e))?;
    }
}
*/
