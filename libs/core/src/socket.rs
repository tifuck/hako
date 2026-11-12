//! Length-prefixed JSON framing over Unix streams.

use crate::MAX_FRAME_BYTES;
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub fn encode_frame(body: &[u8]) -> Result<Vec<u8>> {
    if body.len() > MAX_FRAME_BYTES {
        bail!("frame too large");
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

pub async fn write_json_frame<T: serde::Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let frame = encode_frame(&body)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read_json_frame(stream: &mut UnixStream) -> Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("read frame length")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        bail!("frame length {len} exceeds max");
    }
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .context("read frame body")?;
    let v = serde_json::from_slice(&body)?;
    Ok(v)
}

/// Blocking variants for CLI helpers.
pub fn write_json_frame_sync<T: serde::Serialize, W: Write>(w: &mut W, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let frame = encode_frame(&body)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

pub fn read_json_frame_sync<R: Read>(r: &mut R) -> Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        bail!("frame too large");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}
