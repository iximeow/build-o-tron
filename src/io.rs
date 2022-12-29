use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures_util::StreamExt;
use tokio::fs::File;
use std::io::Write;
use tokio::fs::OpenOptions;

pub struct ArtifactStream {
    sender: hyper::body::Sender,
}

impl ArtifactStream {
    pub fn new(sender: hyper::body::Sender) -> Self {
        Self { sender }
    }
}

pub struct ArtifactDescriptor {
    job_id: u64,
    artifact_id: u64,
    file: File,
}

impl ArtifactDescriptor {
    pub async fn new(job_id: u64, artifact_id: u64) -> Result<Self, String> {
        // TODO: jobs should be a configurable path
        let path = format!("jobs/{}/{}", job_id, artifact_id);
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
