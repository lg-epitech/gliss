//! QEMU's VNC audio extension, negotiated with pseudo-encoding -259.
//! Wire layout: https://gitlab.com/qemu-project/qemu/-/blob/master/ui/vnc.c
use std::{sync::Arc, time::Instant};

use anyhow::{Result, bail, ensure};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::{broadcast, watch},
};

pub(crate) struct Sink {
    pub events: broadcast::Sender<Event>,
    pub available: watch::Sender<bool>,
}

pub const ENCODING: i32 = -259;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
/// At most 341 ms of stereo s16le, bounded before allocating the wire payload.
pub const MAX_CHUNK_BYTES: usize = 65_536;

#[derive(Clone, Debug)]
pub enum Event {
    Begin,
    End,
    Data { bytes: Arc<[u8]>, received: Instant },
}

pub(crate) fn control(enabled: bool) -> Vec<u8> {
    if !enabled {
        return vec![255, 1, 0, 1];
    }
    // QEMU format 3 = signed 16-bit little endian, stereo, 48 kHz.
    let mut bytes = vec![255, 1, 0, 2, 3, CHANNELS];
    bytes.extend_from_slice(&SAMPLE_RATE.to_be_bytes());
    bytes.extend_from_slice(&[255, 1, 0, 0]);
    bytes
}

pub(crate) async fn read<R: AsyncRead + Unpin>(read: &mut R) -> Result<Event> {
    ensure!(
        read.read_u8().await? == 1,
        "unsupported QEMU server message"
    );
    match read.read_u16().await? {
        0 => Ok(Event::End),
        1 => Ok(Event::Begin),
        2 => {
            let length = read.read_u32().await? as usize;
            ensure!(length <= MAX_CHUNK_BYTES, "QEMU audio exceeds chunk limit");
            ensure!(length.is_multiple_of(4), "QEMU audio is not stereo s16le");
            let mut bytes = vec![0; length];
            read.read_exact(&mut bytes).await?;
            Ok(Event::Data {
                bytes: bytes.into(),
                received: Instant::now(),
            })
        }
        other => bail!("unsupported QEMU audio message {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_excessive_unaligned_truncated_and_unknown_audio_messages() {
        for bytes in [
            vec![1, 0, 2, 0, 1, 0, 1], // Excessive length is rejected before payload allocation.
            vec![1, 0, 2, 0, 0, 0, 3], // Incomplete stereo sample.
            vec![1, 0, 2, 0, 0, 0, 4, 1, 2], // Truncated payload.
            vec![1, 0, 3],             // Unknown audio action.
            vec![2, 0, 0],             // Unknown QEMU subtype.
            vec![1, 0],                // Truncated header.
        ] {
            assert!(read(&mut bytes.as_slice()).await.is_err(), "{bytes:?}");
        }
        assert!(matches!(
            read(&mut [1, 0, 0].as_slice()).await.unwrap(),
            Event::End
        ));
        assert!(matches!(
            read(&mut [1, 0, 1].as_slice()).await.unwrap(),
            Event::Begin
        ));
    }
}
