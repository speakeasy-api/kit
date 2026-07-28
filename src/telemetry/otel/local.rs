use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use zeroize::Zeroize;

use super::{ExportBatch, ExportError, Exporter};

const MAGIC: &[u8; 8] = b"KITOTL01";
const NONCE_LEN: usize = 24;
const LENGTH_LEN: usize = 4;

pub struct DurableLocalExporter {
    path: PathBuf,
    key: [u8; 32],
    max_bytes: usize,
}

impl DurableLocalExporter {
    pub fn open(
        path: impl Into<PathBuf>,
        identity_key: &[u8; 32],
        max_bytes: usize,
    ) -> Result<Self, ExportError> {
        if max_bytes <= MAGIC.len() + LENGTH_LEN + NONCE_LEN + 16 {
            return Err(ExportError(
                "telemetry sink capacity is too small".to_owned(),
            ));
        }
        let path = path.into();
        let parent = path
            .parent()
            .ok_or_else(|| ExportError("telemetry sink has no parent directory".to_owned()))?;
        fs::create_dir_all(parent).map_err(export_io)?;
        let exporter = Self {
            path,
            key: blake3::derive_key("kit durable local telemetry v1", identity_key),
            max_bytes,
        };
        if exporter.path.exists() {
            if fs::metadata(&exporter.path).map_err(export_io)?.len() > max_bytes as u64 {
                return Err(ExportError(
                    "existing telemetry sink exceeds configured capacity".to_owned(),
                ));
            }
            exporter.read_batches()?;
        } else {
            exporter.replace(MAGIC)?;
        }
        Ok(exporter)
    }

    pub fn read_batches(&self) -> Result<Vec<ExportBatch>, ExportError> {
        let bytes = fs::read(&self.path).map_err(export_io)?;
        let frames = frames(&bytes)?;
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        frames
            .into_iter()
            .map(|frame| {
                let (nonce, ciphertext) = frame.split_at(NONCE_LEN);
                let plaintext = cipher
                    .decrypt(
                        XNonce::from_slice(nonce),
                        Payload {
                            msg: ciphertext,
                            aad: MAGIC,
                        },
                    )
                    .map_err(|_| ExportError("telemetry sink decryption failed".to_owned()))?;
                let batch: ExportBatch = serde_json::from_slice(&plaintext).map_err(|error| {
                    ExportError(format!("invalid telemetry sink batch: {error}"))
                })?;
                batch.validate().map_err(|error| {
                    ExportError(format!("invalid telemetry sink batch: {error}"))
                })?;
                Ok(batch)
            })
            .collect()
    }

    fn replace(&self, bytes: &[u8]) -> Result<(), ExportError> {
        let parent = self.path.parent().expect("validated telemetry parent");
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random)
            .map_err(|error| ExportError(format!("telemetry randomness failed: {error}")))?;
        let temporary = parent.join(format!(
            ".{}.{}-{}.tmp",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("telemetry"),
            std::process::id(),
            u64::from_ne_bytes(random)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> io::Result<()> {
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(export_io)
    }
}

impl Exporter for DurableLocalExporter {
    fn export(&mut self, batch: &ExportBatch) -> Result<(), ExportError> {
        let mut plaintext = batch
            .to_canonical_json()
            .map_err(|error| ExportError(format!("telemetry serialization failed: {error}")))?;
        let mut nonce = [0_u8; NONCE_LEN];
        getrandom::fill(&mut nonce)
            .map_err(|error| ExportError(format!("telemetry randomness failed: {error}")))?;
        let ciphertext = XChaCha20Poly1305::new((&self.key).into())
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: MAGIC,
                },
            )
            .map_err(|_| ExportError("telemetry encryption failed".to_owned()));
        plaintext.zeroize();
        let ciphertext = ciphertext?;
        let frame_len = nonce.len() + ciphertext.len();
        let frame_len = u32::try_from(frame_len)
            .map_err(|_| ExportError("telemetry batch is too large".to_owned()))?;
        let mut frame = Vec::with_capacity(LENGTH_LEN + frame_len as usize);
        frame.extend_from_slice(&frame_len.to_be_bytes());
        frame.extend_from_slice(&nonce);
        frame.extend_from_slice(&ciphertext);
        if MAGIC.len() + frame.len() > self.max_bytes {
            return Err(ExportError(
                "telemetry batch exceeds local sink capacity".to_owned(),
            ));
        }

        let existing = fs::read(&self.path).map_err(export_io)?;
        let old_frames = frames(&existing)?;
        let retained_len = self.max_bytes - MAGIC.len() - frame.len();
        let mut retained = old_frames
            .iter()
            .rev()
            .scan(0_usize, |used, old| {
                let size = LENGTH_LEN + old.len();
                if *used + size > retained_len {
                    None
                } else {
                    *used += size;
                    Some(*old)
                }
            })
            .collect::<Vec<_>>();
        retained.reverse();
        let mut bytes = Vec::with_capacity(self.max_bytes.min(existing.len() + frame.len()));
        bytes.extend_from_slice(MAGIC);
        for old in retained {
            bytes.extend_from_slice(&(old.len() as u32).to_be_bytes());
            bytes.extend_from_slice(old);
        }
        bytes.extend_from_slice(&frame);
        self.replace(&bytes)
    }
}

impl Drop for DurableLocalExporter {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

fn frames(bytes: &[u8]) -> Result<Vec<&[u8]>, ExportError> {
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(ExportError("invalid telemetry sink header".to_owned()));
    }
    let mut frames = Vec::new();
    let mut offset = MAGIC.len();
    while offset < bytes.len() {
        if bytes.len() - offset < LENGTH_LEN {
            return Err(ExportError("truncated telemetry sink frame".to_owned()));
        }
        let length = u32::from_be_bytes(
            bytes[offset..offset + LENGTH_LEN]
                .try_into()
                .expect("frame length slice"),
        ) as usize;
        offset += LENGTH_LEN;
        if length < NONCE_LEN + 16 || bytes.len() - offset < length {
            return Err(ExportError("invalid telemetry sink frame".to_owned()));
        }
        frames.push(&bytes[offset..offset + length]);
        offset += length;
    }
    Ok(frames)
}

fn export_io(error: io::Error) -> ExportError {
    ExportError(format!("telemetry sink I/O failed: {error}"))
}
