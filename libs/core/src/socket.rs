//! Length-prefixed JSON framing over Unix streams.

use crate::MAX_FRAME_BYTES;
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Longest a peer may take to finish a frame it started sending.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// A body over `MAX_FRAME_BYTES`. Raised before anything is written, so the
/// stream is still in sync and can carry another frame.
#[derive(Debug)]
pub struct FrameTooLarge(pub usize);

impl std::fmt::Display for FrameTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "frame too large ({} bytes)", self.0)
    }
}

impl std::error::Error for FrameTooLarge {}

pub fn encode_frame(body: &[u8]) -> Result<Vec<u8>> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameTooLarge(body.len()).into());
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

/// Waits as long as it takes for a frame to start; a started frame must be
/// complete within `FRAME_TIMEOUT` (a stalled peer does not hold its buffer).
pub async fn read_json_frame(stream: &mut UnixStream) -> Result<serde_json::Value> {
    read_frame_within(stream, FRAME_TIMEOUT).await
}

async fn read_frame_within(stream: &mut UnixStream, limit: Duration) -> Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf[..1])
        .await
        .context("read frame length")?;
    let body = tokio::time::timeout(limit, async {
        stream
            .read_exact(&mut len_buf[1..])
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
        Ok(body)
    })
    .await
    .map_err(|_| anyhow::anyhow!("frame not completed within {limit:?}"))??;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_up_to_the_max_encode() {
        assert!(encode_frame(&vec![b' '; MAX_FRAME_BYTES]).is_ok());
        let e = encode_frame(&vec![b' '; MAX_FRAME_BYTES + 1]).unwrap_err();
        assert!(e.is::<FrameTooLarge>(), "{e:#}");
    }

    #[tokio::test]
    async fn a_header_of_the_max_is_accepted_and_one_more_refused() {
        let limit = Duration::from_millis(100);
        let header = |len: usize| async move {
            let (mut server, mut client) = UnixStream::pair().unwrap();
            client.write_all(&(len as u32).to_le_bytes()).await.unwrap();
            // No body: an accepted length waits for it and times out.
            let e = read_frame_within(&mut server, limit).await.unwrap_err();
            format!("{e:#}")
        };
        let max = header(MAX_FRAME_BYTES).await;
        assert!(max.contains("not completed"), "{max}");
        let over = header(MAX_FRAME_BYTES + 1).await;
        assert!(over.contains("exceeds max"), "{over}");
    }

    #[tokio::test]
    async fn a_stalled_frame_times_out() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        client.write_all(&100u32.to_le_bytes()).await.unwrap();
        client.write_all(b"{").await.unwrap();
        let limit = Duration::from_millis(100);
        let e = read_frame_within(&mut server, limit).await.unwrap_err();
        assert!(format!("{e:#}").contains("not completed"), "{e:#}");

        // A stall inside the length header counts too.
        let (mut server, mut client) = UnixStream::pair().unwrap();
        client.write_all(&[100]).await.unwrap();
        let e = read_frame_within(&mut server, limit).await.unwrap_err();
        assert!(format!("{e:#}").contains("not completed"), "{e:#}");

        // A whole frame still reads, however late it starts.
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let late = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            write_json_frame(&mut client, &serde_json::json!({"a": 1})).await
        };
        let (read, wrote) = tokio::join!(read_frame_within(&mut server, limit), late);
        wrote.unwrap();
        assert_eq!(read.unwrap(), serde_json::json!({"a": 1}));
    }
}
