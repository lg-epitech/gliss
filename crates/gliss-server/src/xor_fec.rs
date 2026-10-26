pub(crate) const DATA_FRAGMENTS_PER_GROUP: usize = 8;
pub(crate) const DATAGRAM_HEADER_BYTES: usize = 20;
pub(crate) const BROWSER_DATAGRAM_HIGH_WATER_MARK: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FrameLayout {
    pub(crate) data_datagrams: usize,
    pub(crate) parity_datagrams: usize,
    pub(crate) data_application_bytes: usize,
    pub(crate) parity_application_bytes: usize,
    pub(crate) queued_bytes: usize,
}

impl FrameLayout {
    pub(crate) fn datagrams(self) -> usize {
        self.data_datagrams.saturating_add(self.parity_datagrams)
    }
}

pub(crate) fn frame_layout(
    total: usize,
    stride: usize,
    session_header_bytes: usize,
    metadata_bytes: usize,
) -> Option<FrameLayout> {
    if total == 0 || stride == 0 {
        return None;
    }
    let data_datagrams = total.div_ceil(stride);
    if data_datagrams > usize::from(u16::MAX) {
        return None;
    }
    let parity_datagrams = data_datagrams.div_ceil(DATA_FRAGMENTS_PER_GROUP);
    let mut parity_payload_bytes = 0usize;
    for first in (0..data_datagrams).step_by(DATA_FRAGMENTS_PER_GROUP) {
        let offset = first.checked_mul(stride)?;
        parity_payload_bytes =
            parity_payload_bytes.checked_add(total.checked_sub(offset)?.min(stride))?;
    }
    let data_application_bytes =
        total.checked_add(data_datagrams.checked_mul(DATAGRAM_HEADER_BYTES)?)?;
    let parity_application_bytes =
        parity_payload_bytes.checked_add(parity_datagrams.checked_mul(DATAGRAM_HEADER_BYTES)?)?;
    let transport_bytes = data_datagrams
        .checked_add(parity_datagrams)?
        .checked_mul(session_header_bytes.checked_add(metadata_bytes)?)?;
    Some(FrameLayout {
        data_datagrams,
        parity_datagrams,
        data_application_bytes,
        parity_application_bytes,
        queued_bytes: data_application_bytes
            .checked_add(parity_application_bytes)?
            .checked_add(transport_bytes)?,
    })
}

// At most the final data fragment and a one-fragment final parity group are
// shorter than stride. Every other datagram charges at least stride plus the
// fixed header/session/metadata extent.
pub(crate) fn admitted_datagram_upper_bound(
    send_buffer_bytes: usize,
    minimum_stride: usize,
    fixed_bytes_per_datagram: usize,
) -> usize {
    send_buffer_bytes.saturating_add(minimum_stride.saturating_mul(2))
        / minimum_stride.saturating_add(fixed_bytes_per_datagram)
}

pub(crate) struct ParityDatagram {
    pub(crate) bytes: Vec<u8>,
    pub(crate) payload_bytes_read: usize,
}

pub(crate) fn parity_datagram(
    bytes: &[u8],
    sequence: u32,
    stride: usize,
    first: usize,
    scratch: &mut [u8],
) -> Option<ParityDatagram> {
    if bytes.is_empty()
        || stride == 0
        || stride > usize::from(u16::MAX)
        || scratch.len() != stride
        || !first.is_multiple_of(DATA_FRAGMENTS_PER_GROUP)
    {
        return None;
    }
    let count = bytes.len().div_ceil(stride);
    if count > usize::from(u16::MAX) || first >= count || first > usize::from(u16::MAX) {
        return None;
    }
    let group_size = (count - first).min(DATA_FRAGMENTS_PER_GROUP);
    let first_offset = first.checked_mul(stride)?;
    let parity_payload_bytes = (bytes.len() - first_offset).min(stride);
    scratch[..parity_payload_bytes].fill(0);
    let mut payload_bytes_read = 0usize;
    for index in first..first + group_size {
        let offset = index.checked_mul(stride)?;
        let payload = &bytes[offset..bytes.len().min(offset.checked_add(stride)?)];
        payload_bytes_read = payload_bytes_read.checked_add(payload.len())?;
        for (target, value) in scratch.iter_mut().zip(payload) {
            *target ^= *value;
        }
    }
    let total = u32::try_from(bytes.len()).ok()?;
    let mut output = Vec::with_capacity(DATAGRAM_HEADER_BYTES + parity_payload_bytes);
    output.extend_from_slice(b"GLP1");
    output.extend_from_slice(&sequence.to_le_bytes());
    output.extend_from_slice(&total.to_le_bytes());
    output.extend_from_slice(&(first as u16).to_le_bytes());
    output.extend_from_slice(&(count as u16).to_le_bytes());
    output.extend_from_slice(&(stride as u16).to_le_bytes());
    output.push(group_size as u8);
    output.push(0);
    output.extend_from_slice(&scratch[..parity_payload_bytes]);
    Some(ParityDatagram {
        bytes: output,
        payload_bytes_read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_group_has_canonical_header_and_xor_payload() {
        let stride = 256;
        let mut source = vec![0u8; 8 * stride];
        for (index, chunk) in source.chunks_mut(stride).enumerate() {
            chunk.fill((index + 1) as u8);
        }
        let mut scratch = vec![0u8; stride];
        let parity = parity_datagram(&source, 0x1020_3040, stride, 0, &mut scratch).unwrap();
        let view = &parity.bytes;
        assert_eq!(&view[..4], b"GLP1");
        assert_eq!(
            u32::from_le_bytes(view[4..8].try_into().unwrap()),
            0x1020_3040
        );
        assert_eq!(u32::from_le_bytes(view[8..12].try_into().unwrap()), 2048);
        assert_eq!(u16::from_le_bytes(view[12..14].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(view[14..16].try_into().unwrap()), 8);
        assert_eq!(u16::from_le_bytes(view[16..18].try_into().unwrap()), 256);
        assert_eq!(&view[18..20], &[8, 0]);
        assert_eq!(view.len(), 276);
        assert!(view[20..].iter().all(|value| *value == 8));
        assert_eq!(parity.payload_bytes_read, source.len());
    }

    #[test]
    fn short_tail_group_zero_pads_only_the_short_fragment() {
        let stride = 256;
        let mut source = vec![0u8; 9 * stride + 37];
        source[8 * stride..9 * stride].fill(0x55);
        source[9 * stride..].fill(0x0f);
        let mut scratch = vec![0u8; stride];
        let parity = parity_datagram(&source, 7, stride, 8, &mut scratch).unwrap();
        assert_eq!(
            u32::from_le_bytes(parity.bytes[8..12].try_into().unwrap()),
            source.len() as u32
        );
        assert_eq!(
            u16::from_le_bytes(parity.bytes[12..14].try_into().unwrap()),
            8
        );
        assert_eq!(
            u16::from_le_bytes(parity.bytes[14..16].try_into().unwrap()),
            10
        );
        assert_eq!(&parity.bytes[18..20], &[2, 0]);
        assert_eq!(parity.bytes.len(), DATAGRAM_HEADER_BYTES + stride);
        assert!(parity.bytes[20..57].iter().all(|value| *value == 0x5a));
        assert!(parity.bytes[57..].iter().all(|value| *value == 0x55));
        assert_eq!(parity.payload_bytes_read, stride + 37);
    }

    #[test]
    fn layout_charges_parity_and_proves_the_browser_queue_bound() {
        let layout = frame_layout(900_000, 1000, 8, 128).unwrap();
        assert_eq!(layout.data_datagrams, 900);
        assert_eq!(layout.parity_datagrams, 113);
        assert_eq!(layout.data_application_bytes, 918_000);
        assert_eq!(layout.parity_application_bytes, 115_260);
        assert_eq!(layout.queued_bytes, 1_171_028);
        assert_eq!(layout.datagrams(), 1013);
        let bound = admitted_datagram_upper_bound(1024 * 1024, 256, 20 + 8 + 128);
        assert_eq!(bound, 2546);
        assert!(bound < BROWSER_DATAGRAM_HIGH_WATER_MARK);
    }
}
