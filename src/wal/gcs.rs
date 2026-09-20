use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_storage::appendable_object_writer::AppendableObjectWriter;
use google_cloud_storage::client::{Storage, StorageControl};
use tracing::{debug, info};

use crate::wal::backend::{ActiveWalWriter, WalBackend};
use crate::wal::proto::WalEntry;

pub struct GcsActiveWalWriter {
    writer: AppendableObjectWriter,
}

#[async_trait]
impl ActiveWalWriter for GcsActiveWalWriter {
    async fn append_entry(&mut self, entry: &WalEntry) -> Result<()> {
        let framed = entry.encode_framed();
        self.writer
            .append(framed)
            .await
            .context("Appending WalEntry to GCS Rapid Bucket writer")?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .await
            .context("Flushing WalEntry to GCS Rapid Bucket")?;
        Ok(())
    }

    async fn finalize(self: Box<Self>) -> Result<()> {
        self.writer
            .finalize()
            .await
            .context("Finalizing GCS Rapid Bucket appendable object")?;
        Ok(())
    }
}

pub struct GcsRapidWalBackend {
    storage: Storage,
    control: StorageControl,
    bucket: String,
    prefix: String,
}

impl GcsRapidWalBackend {
    pub async fn new(bucket: &str, prefix: &str) -> Result<Self> {
        let storage = Storage::builder().build().await.context("Building Storage client")?;
        let control = StorageControl::builder().build().await.context("Building StorageControl client")?;

        let clean_prefix = prefix.trim_start_matches('/').to_string();
        let formatted_bucket = if bucket.starts_with("projects/") {
            bucket.to_string()
        } else {
            format!("projects/_/buckets/{bucket}")
        };

        Ok(Self {
            storage,
            control,
            bucket: formatted_bucket,
            prefix: clean_prefix,
        })
    }

    fn log_name(&self, seq: u64) -> String {
        format!("{}{seq}.log", self.prefix)
    }

    fn done_name(&self, seq: u64) -> String {
        format!("{}{seq}.done", self.prefix)
    }

    fn pack_name(&self, seq: u64) -> String {
        format!("{}{seq}.pack", self.prefix)
    }

    fn idx_name(&self, seq: u64) -> String {
        format!("{}{seq}.idx", self.prefix)
    }

    pub async fn write_rapid_object(&self, object_name: &str, data: &[u8]) -> Result<()> {
        let mut writer = self
            .storage
            .open_appendable_object(&self.bucket, object_name)
            .send()
            .await
            .with_context(|| format!("Opening appendable object {} in {}", object_name, self.bucket))?;

        if !data.is_empty() {
            writer.append(Bytes::copy_from_slice(data)).await?;
            writer.flush().await?;
        }
        writer.finalize().await?;
        Ok(())
    }
}

#[async_trait]
impl WalBackend for GcsRapidWalBackend {
    async fn list_log_sequences(&self) -> Result<Vec<u64>> {
        let mut seqs = Vec::new();
        let mut list_builder = self.control.list_objects().set_parent(&self.bucket);
        if !self.prefix.is_empty() {
            list_builder = list_builder.set_prefix(&self.prefix);
        }

        let resp = list_builder
            .send()
            .await
            .context(format!("Listing objects in bucket {}", self.bucket))?;

        for obj in resp.objects {
            let name = obj.name;
            let stripped = if !self.prefix.is_empty() {
                name.strip_prefix(&self.prefix).unwrap_or(&name)
            } else {
                &name
            };

            if let Some(num_str) = stripped.strip_suffix(".log") {
                if let Ok(seq) = num_str.parse::<u64>() {
                    seqs.push(seq);
                }
            }
        }

        seqs.sort_unstable();
        Ok(seqs)
    }

    async fn is_log_done(&self, seq: u64) -> Result<bool> {
        let done_name = self.done_name(seq);
        let resp = self
            .control
            .get_object()
            .set_bucket(&self.bucket)
            .set_object(done_name)
            .send()
            .await;

        match resp {
            Ok(_) => Ok(true),
            Err(e) => {
                debug!("Object get for done marker of WAL {seq} returned: {:?}", e);
                Ok(false)
            }
        }
    }

    async fn mark_log_done(
        &self,
        seq: u64,
        new_commit_oid: &str,
        pack_data: Option<&[u8]>,
        idx_data: Option<&[u8]>,
    ) -> Result<()> {
        let done_name = self.done_name(seq);
        let done_payload = format!("{new_commit_oid}\n").into_bytes();
        self.write_rapid_object(&done_name, &done_payload)
            .await
            .context(format!("Writing done marker for WAL {seq} in GCS"))?;

        if let Some(pack) = pack_data {
            let pack_name = self.pack_name(seq);
            self.write_rapid_object(&pack_name, pack)
                .await
                .context(format!("Uploading packfile for WAL {seq} to GCS"))?;
        }

        if let Some(idx) = idx_data {
            let idx_name = self.idx_name(seq);
            self.write_rapid_object(&idx_name, idx)
                .await
                .context(format!("Uploading idx for WAL {seq} to GCS"))?;
        }

        info!("Successfully marked WAL {seq} as completed in GCS Rapid Bucket");
        Ok(())
    }

    async fn lockout_zombies(&self, previous_seqs: &[u64]) -> Result<()> {
        for &seq in previous_seqs {
            let obj_name = self.log_name(seq);
            // Fetch object generation
            match self
                .control
                .get_object()
                .set_bucket(&self.bucket)
                .set_object(&obj_name)
                .send()
                .await
            {
                Ok(obj) => {
                    let generation = obj.generation;
                    debug!(
                        "Locking out zombie servers on WAL {seq} (generation {generation})..."
                    );
                    match self
                        .storage
                        .reopen_appendable_object(&self.bucket, &obj_name, generation)
                        .send()
                        .await
                    {
                        Ok(writer) => match writer.finalize().await {
                            Ok(_) => {
                                info!("Successfully finalized prior WAL {seq} (zombie locked out)");
                            }
                            Err(e) => {
                                debug!("Finalizing prior WAL {seq} failed (may already be finalized): {e}");
                            }
                        },
                        Err(e) => {
                            debug!("Reopening prior WAL {seq} failed (may already be sealed): {e}");
                        }
                    }
                }
                Err(e) => {
                    debug!("Could not get metadata for prior WAL {seq}: {e}");
                }
            }
        }
        Ok(())
    }

    async fn open_writer(&self, seq: u64) -> Result<Box<dyn ActiveWalWriter>> {
        let obj_name = self.log_name(seq);
        let writer = self
            .storage
            .open_appendable_object(&self.bucket, &obj_name)
            .send()
            .await
            .with_context(|| format!("Opening appendable object {} in {}", obj_name, self.bucket))?;

        info!("Opened new GCS Rapid Bucket WAL stream: {}", obj_name);
        Ok(Box::new(GcsActiveWalWriter { writer }))
    }

    async fn read_log(&self, seq: u64) -> Result<Vec<u8>> {
        let obj_name = self.log_name(seq);
        let mut reader = self
            .storage
            .read_object(&self.bucket, &obj_name)
            .send()
            .await
            .with_context(|| format!("Reading object {} from bucket {}", obj_name, self.bucket))?;

        let mut data = Vec::new();
        while let Some(chunk_res) = reader.next().await {
            let chunk = chunk_res.context("Streaming chunk from GCS")?;
            data.extend_from_slice(&chunk);
        }

        Ok(data)
    }
}
