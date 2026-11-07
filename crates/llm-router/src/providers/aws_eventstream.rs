//! Bounded incremental AWS EventStream frames. Verify both CRCs before delivery.
use crate::ProviderError;
use std::collections::BTreeMap;

/// Header values retain their wire type. Unknown headers can be skipped safely.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Integer(i32),
    Long(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}
impl HeaderValue {
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(s) = self {
            Some(s)
        } else {
            None
        }
    }
}
/// Debug deliberately omits the provider payload and header values.
pub struct EventFrame {
    pub headers: BTreeMap<String, HeaderValue>,
    pub payload: Vec<u8>,
}
impl std::fmt::Debug for EventFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventFrame")
            .field("headers", &self.headers.len())
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}
impl EventFrame {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(HeaderValue::as_str)
    }
}
#[derive(Default)]
pub struct FrameDecoder {
    pending: Vec<u8>,
    total: usize,
    failed: bool,
}
impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn feed(
        &mut self,
        bytes: &[u8],
        sink: &mut dyn FnMut(EventFrame) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        if self.failed {
            return Err(ProviderError::Permanent);
        }
        let result = self.feed_inner(bytes, sink);
        if result.is_err() {
            self.failed = true;
            self.pending.clear();
        }
        result
    }
    fn feed_inner(
        &mut self,
        mut bytes: &[u8],
        sink: &mut dyn FnMut(EventFrame) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        self.total = self
            .total
            .checked_add(bytes.len())
            .filter(|n| *n <= super::openai_compatible::MAX_HTTP_RESPONSE_BYTES)
            .ok_or(ProviderError::Permanent)?;
        while !bytes.is_empty() {
            // Only buffer the current frame, never allocate from an unchecked length.
            let want = if self.pending.len() < 12 {
                12 - self.pending.len()
            } else {
                frame_lengths(&self.pending)?.0 - self.pending.len()
            };
            let take = bytes.len().min(want);
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() < 12 {
                continue;
            }
            let (length, headers) = frame_lengths(&self.pending)?;
            if self.pending.len() < length {
                continue;
            }
            let expected = u32::from_be_bytes(
                self.pending[length - 4..length]
                    .try_into()
                    .map_err(|_| ProviderError::Permanent)?,
            );
            if crc32fast::hash(&self.pending[..length - 4]) != expected {
                return Err(ProviderError::Permanent);
            }
            let parsed = parse_headers(&self.pending[12..12 + headers])?;
            let payload = self.pending[12 + headers..length - 4].to_vec();
            sink(EventFrame {
                headers: parsed,
                payload,
            })?;
            self.pending.clear();
        }
        Ok(())
    }
    pub fn finish(&self) -> Result<(), ProviderError> {
        if self.failed || !self.pending.is_empty() {
            Err(ProviderError::Permanent)
        } else {
            Ok(())
        }
    }
}
fn frame_lengths(prelude: &[u8]) -> Result<(usize, usize), ProviderError> {
    if prelude.len() < 12 {
        return Err(ProviderError::Permanent);
    }
    let length = u32::from_be_bytes(
        prelude[..4]
            .try_into()
            .map_err(|_| ProviderError::Permanent)?,
    ) as usize;
    let headers = u32::from_be_bytes(
        prelude[4..8]
            .try_into()
            .map_err(|_| ProviderError::Permanent)?,
    ) as usize;
    let crc = u32::from_be_bytes(
        prelude[8..12]
            .try_into()
            .map_err(|_| ProviderError::Permanent)?,
    );
    if crc32fast::hash(&prelude[..8]) != crc
        || !(16..=super::openai_compatible::MAX_HTTP_RESPONSE_BYTES).contains(&length)
        || headers > length - 16
    {
        return Err(ProviderError::Permanent);
    }
    Ok((length, headers))
}
fn take<'a>(bytes: &mut &'a [u8], length: usize) -> Result<&'a [u8], ProviderError> {
    let (head, tail) = bytes
        .split_at_checked(length)
        .ok_or(ProviderError::Permanent)?;
    *bytes = tail;
    Ok(head)
}
fn parse_headers(mut bytes: &[u8]) -> Result<BTreeMap<String, HeaderValue>, ProviderError> {
    let mut headers = BTreeMap::new();
    while !bytes.is_empty() {
        if headers.len() >= 64 {
            return Err(ProviderError::Permanent);
        }
        let length = take(&mut bytes, 1)?[0] as usize;
        if length == 0 {
            return Err(ProviderError::Permanent);
        }
        let name = std::str::from_utf8(take(&mut bytes, length)?)
            .map_err(|_| ProviderError::Permanent)?
            .to_owned();
        let kind = take(&mut bytes, 1)?[0];
        let value = match kind {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Byte(take(&mut bytes, 1)?[0] as i8),
            3 => HeaderValue::Short(i16::from_be_bytes(
                take(&mut bytes, 2)?
                    .try_into()
                    .map_err(|_| ProviderError::Permanent)?,
            )),
            4 => HeaderValue::Integer(i32::from_be_bytes(
                take(&mut bytes, 4)?
                    .try_into()
                    .map_err(|_| ProviderError::Permanent)?,
            )),
            5 | 8 => {
                let v = i64::from_be_bytes(
                    take(&mut bytes, 8)?
                        .try_into()
                        .map_err(|_| ProviderError::Permanent)?,
                );
                if kind == 5 {
                    HeaderValue::Long(v)
                } else {
                    HeaderValue::Timestamp(v)
                }
            }
            6 | 7 => {
                let length = u16::from_be_bytes(
                    take(&mut bytes, 2)?
                        .try_into()
                        .map_err(|_| ProviderError::Permanent)?,
                ) as usize;
                let v = take(&mut bytes, length)?;
                if kind == 6 {
                    HeaderValue::Bytes(v.to_vec())
                } else {
                    HeaderValue::String(
                        std::str::from_utf8(v)
                            .map_err(|_| ProviderError::Permanent)?
                            .to_owned(),
                    )
                }
            }
            9 => HeaderValue::Uuid(
                take(&mut bytes, 16)?
                    .try_into()
                    .map_err(|_| ProviderError::Permanent)?,
            ),
            _ => return Err(ProviderError::Permanent),
        };
        if headers.insert(name, value).is_some() {
            return Err(ProviderError::Permanent);
        }
    }
    Ok(headers)
}

#[cfg(test)]
pub(crate) fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (name, value) in headers {
        encoded.push(name.len() as u8);
        encoded.extend_from_slice(name.as_bytes());
        encoded.push(7);
        encoded.extend_from_slice(&(value.len() as u16).to_be_bytes());
        encoded.extend_from_slice(value.as_bytes());
    }
    let total = 16 + encoded.len() + payload.len();
    let mut frame = Vec::new();
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame.extend_from_slice(&encoded);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aws_event_frames_survive_every_split_and_never_decode_binary_as_utf8() {
        let frame = encode_frame(
            &[
                (":message-type", "event"),
                (":event-type", "contentBlockDelta"),
            ],
            "{\"delta\":{\"text\":\"🐈\"}}".as_bytes(),
        );
        for split in 0..=frame.len() {
            let mut decoder = FrameDecoder::new();
            let mut frames = Vec::new();
            let mut sink = |f| {
                frames.push(f);
                Ok(())
            };
            decoder.feed(&frame[..split], &mut sink).unwrap();
            decoder.feed(&frame[split..], &mut sink).unwrap();
            decoder.finish().unwrap();
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].header(":event-type"), Some("contentBlockDelta"));
            assert!(
                std::str::from_utf8(&frames[0].payload)
                    .unwrap()
                    .contains('🐈')
            );
        }
        let mut decoder = FrameDecoder::new();
        let mut count = 0;
        let mut both = frame.clone();
        both.extend_from_slice(&frame);
        decoder
            .feed(&both, &mut |_| {
                count += 1;
                Ok(())
            })
            .unwrap();
        decoder.finish().unwrap();
        assert_eq!(count, 2);
    }
    #[test]
    fn aws_frames_refuse_bad_crc_lengths_headers_truncation_and_callback_errors() {
        let frame = encode_frame(&[(":message-type", "event")], b"private-payload");
        for at in [0, 4, 8, 12, frame.len() - 1] {
            let mut bad = frame.clone();
            bad[at] ^= 1;
            let mut decoder = FrameDecoder::new();
            let mut count = 0;
            assert!(
                decoder
                    .feed(&bad, &mut |_| {
                        count += 1;
                        Ok(())
                    })
                    .is_err()
            );
            assert_eq!(count, 0);
            assert!(decoder.finish().is_err());
        }
        for length in 1..frame.len() {
            let mut decoder = FrameDecoder::new();
            decoder.feed(&frame[..length], &mut |_| Ok(())).unwrap();
            assert!(decoder.finish().is_err());
        }
        let duplicate = encode_frame(&[("x", "one"), ("x", "two")], b"{}");
        assert!(
            FrameDecoder::new()
                .feed(&duplicate, &mut |_| Ok(()))
                .is_err()
        );
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.feed(&frame, &mut |_| Err(ProviderError::Cancelled)),
            Err(ProviderError::Cancelled)
        );
        assert!(decoder.finish().is_err());
        assert!(
            !format!(
                "{:?}",
                EventFrame {
                    headers: BTreeMap::new(),
                    payload: b"private-payload".to_vec()
                }
            )
            .contains("private-payload")
        );
    }
}
