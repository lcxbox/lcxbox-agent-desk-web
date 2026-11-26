//! Chunk a serialized remote-tool result into signaling-sized frames and
//! reassemble it on the other side.
//!
//! The edge slices the already-serialized bytes, base64-encodes each slice, and
//! ships one [`RemoteToolResponseChunk`] per slice. The final chunk carries the
//! total byte length and the hex SHA-256 of the whole stream. The receiver
//! ([`ByteReassembler`]) verifies ordering, length, and hash before handing the
//! bytes back.

use desk_agent_protocol::remote_tool::RemoteToolResponseChunk;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

/// Why a chunked byte stream could not be reassembled.
#[derive(Debug)]
pub enum ChunkError {
    /// A chunk's base64 payload did not decode.
    Base64(base64::DecodeError),
    /// No chunks were received.
    Empty,
    /// A chunk arrived with an unexpected `seq` (not strictly increasing from 0).
    SeqGap { expected: u32, got: u32 },
    /// The final chunk was missing (no chunk had `last = true`).
    MissingFinal,
    /// The reassembled byte length did not match the declared `total_len`.
    TotalLenMismatch { declared: u64, actual: u64 },
    /// The final chunk carried no SHA-256.
    MissingHash,
    /// The reassembled bytes' SHA-256 did not match the declared hash.
    HashMismatch { declared: String, actual: String },
    /// The declared `total_len` exceeds the receiver's hard upper bound (checked
    /// before allocating, so a forged length cannot drive an unbounded buffer).
    TotalLenTooLarge { declared: u64, limit: u64 },
    /// A non-final chunk carried a SHA-256 (only the final chunk may).
    HashOnNonFinal { seq: u32 },
    /// Two chunks declared different `total_len` values.
    InconsistentTotalLen { first: u64, got: u64 },
    /// A chunk arrived after the final (`last = true`) chunk was already seen.
    ChunkAfterFinal { seq: u32 },
    /// The accumulated decoded bytes exceeded the declared `total_len`.
    OverDeclaredLen { declared: u64, accumulated: u64 },
    /// A chunk's encoded payload cannot possibly fit in the remaining declared
    /// byte count. Rejected before base64 decoding/allocation.
    ChunkPayloadTooLarge { encoded: usize, remaining: u64 },
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Base64(e) => write!(f, "chunk base64 decode failed: {e}"),
            ChunkError::Empty => write!(f, "no chunks received"),
            ChunkError::SeqGap { expected, got } => {
                write!(f, "chunk sequence gap: expected {expected}, got {got}")
            }
            ChunkError::MissingFinal => write!(f, "no final chunk (last=true) received"),
            ChunkError::TotalLenMismatch { declared, actual } => {
                write!(f, "total_len mismatch: declared {declared}, got {actual}")
            }
            ChunkError::MissingHash => write!(f, "final chunk carried no sha256"),
            ChunkError::HashMismatch { declared, actual } => {
                write!(f, "sha256 mismatch: declared {declared}, got {actual}")
            }
            ChunkError::TotalLenTooLarge { declared, limit } => {
                write!(
                    f,
                    "declared total_len {declared} exceeds hard limit {limit}"
                )
            }
            ChunkError::HashOnNonFinal { seq } => {
                write!(f, "non-final chunk {seq} carried a sha256")
            }
            ChunkError::InconsistentTotalLen { first, got } => {
                write!(f, "inconsistent total_len: first {first}, got {got}")
            }
            ChunkError::ChunkAfterFinal { seq } => {
                write!(f, "chunk {seq} arrived after the final chunk")
            }
            ChunkError::OverDeclaredLen {
                declared,
                accumulated,
            } => {
                write!(
                    f,
                    "accumulated {accumulated} bytes exceeds declared total_len {declared}"
                )
            }
            ChunkError::ChunkPayloadTooLarge { encoded, remaining } => write!(
                f,
                "base64 payload length {encoded} cannot fit in remaining {remaining} bytes"
            ),
        }
    }
}

impl std::error::Error for ChunkError {}

/// Hex SHA-256 of a byte slice (lowercase, no separators).
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Raw byte slice size per chunk derived from the base64 text budget: base64
/// inflates 3 bytes into 4 chars, so `limit` base64 chars hold `limit * 3 / 4`
/// raw bytes. Rounded down to a multiple of 3 so no chunk ends mid-group (clean
/// base64 per chunk; reassembly concatenates raw bytes regardless).
fn raw_chunk_size(base64_limit: usize) -> usize {
    let raw = base64_limit / 4 * 3;
    (raw / 3 * 3).max(3)
}

fn reject_encoded_payload_over_remaining(
    encoded_len: usize,
    remaining: u64,
) -> Result<(), ChunkError> {
    let max_encoded = remaining.div_ceil(3).saturating_mul(4);
    if encoded_len as u64 > max_encoded {
        return Err(ChunkError::ChunkPayloadTooLarge {
            encoded: encoded_len,
            remaining,
        });
    }
    Ok(())
}

/// Split arbitrary already-serialized bytes into [`RemoteToolResponseChunk`]s
/// whose base64 payload stays within `base64_limit` characters. Always returns at
/// least one chunk (an empty input still produces a single `last = true` chunk
/// with an empty payload). The edge uses this to ship a serialized
/// [`desk_agent_protocol::remote_tool::RemoteToolOutput`] back to the manager.
pub fn chunk_bytes(
    request_id: &str,
    bytes: &[u8],
    base64_limit: usize,
) -> Vec<RemoteToolResponseChunk> {
    let total_len = bytes.len() as u64;
    let hash = sha256_hex(bytes);
    let step = raw_chunk_size(base64_limit);

    let mut chunks = Vec::new();
    let mut seq: u32 = 0;
    let mut offset = 0usize;
    loop {
        let end = (offset + step).min(bytes.len());
        let slice = &bytes[offset..end];
        let last = end >= bytes.len();
        chunks.push(RemoteToolResponseChunk {
            request_id: request_id.to_string(),
            seq,
            last,
            total_len,
            payload_b64: BASE64.encode(slice),
            sha256: if last { Some(hash.clone()) } else { None },
        });
        if last {
            break;
        }
        offset = end;
        seq += 1;
    }
    chunks
}

/// Stateful accumulator for a B→A remote tool result, stricter than
/// [`SnapshotReassembler`] because the payload is attacker-influenced (a remote
/// tool result), so every framing claim is validated and the declared length is
/// bounded **before** any allocation (§8.2):
///
/// - the first chunk's `total_len` must not exceed `max_total_bytes` (no
///   pre-allocation of an oversized buffer);
/// - every chunk must declare the same `total_len`;
/// - only the final chunk may carry a `sha256`;
/// - no chunk may arrive after the final chunk;
/// - chunks must arrive strictly in `seq` order from 0;
/// - the accumulated decoded length must never exceed the declared `total_len`;
/// - on finish, the length and SHA-256 must match.
pub struct ByteReassembler {
    buf: Vec<u8>,
    next_seq: u32,
    total_len: Option<u64>,
    hash: Option<String>,
    seen_final: bool,
    max_total_bytes: u64,
}

impl ByteReassembler {
    /// A reassembler that rejects any declared `total_len` above `max_total_bytes`.
    pub fn new(max_total_bytes: u64) -> Self {
        Self {
            buf: Vec::new(),
            next_seq: 0,
            total_len: None,
            hash: None,
            seen_final: false,
            max_total_bytes,
        }
    }

    /// Accept one chunk, validating all framing invariants. Chunks must arrive in
    /// `seq` order starting at 0.
    pub fn push(&mut self, chunk: &RemoteToolResponseChunk) -> Result<(), ChunkError> {
        if self.seen_final {
            return Err(ChunkError::ChunkAfterFinal { seq: chunk.seq });
        }
        if chunk.seq != self.next_seq {
            return Err(ChunkError::SeqGap {
                expected: self.next_seq,
                got: chunk.seq,
            });
        }
        if !chunk.last && chunk.sha256.is_some() {
            return Err(ChunkError::HashOnNonFinal { seq: chunk.seq });
        }
        // Bound the declared length before allocating anything for it.
        match self.total_len {
            None => {
                if chunk.total_len > self.max_total_bytes {
                    return Err(ChunkError::TotalLenTooLarge {
                        declared: chunk.total_len,
                        limit: self.max_total_bytes,
                    });
                }
                self.total_len = Some(chunk.total_len);
            }
            Some(first) if first != chunk.total_len => {
                return Err(ChunkError::InconsistentTotalLen {
                    first,
                    got: chunk.total_len,
                });
            }
            Some(_) => {}
        }
        reject_encoded_payload_over_remaining(
            chunk.payload_b64.len(),
            chunk.total_len.saturating_sub(self.buf.len() as u64),
        )?;
        let decoded = BASE64
            .decode(chunk.payload_b64.as_bytes())
            .map_err(ChunkError::Base64)?;
        let accumulated = self.buf.len() as u64 + decoded.len() as u64;
        if let Some(declared) = self.total_len
            && accumulated > declared
        {
            return Err(ChunkError::OverDeclaredLen {
                declared,
                accumulated,
            });
        }
        self.buf.extend_from_slice(&decoded);
        self.next_seq += 1;
        if chunk.last {
            self.seen_final = true;
            self.hash = chunk.sha256.clone();
        }
        Ok(())
    }

    /// Finalize: verify the final chunk was seen, the length and hash match, and
    /// return the reassembled raw bytes (the caller deserializes them).
    pub fn finish(self) -> Result<Vec<u8>, ChunkError> {
        if self.next_seq == 0 {
            return Err(ChunkError::Empty);
        }
        if !self.seen_final {
            return Err(ChunkError::MissingFinal);
        }
        if let Some(declared) = self.total_len
            && declared != self.buf.len() as u64
        {
            return Err(ChunkError::TotalLenMismatch {
                declared,
                actual: self.buf.len() as u64,
            });
        }
        let declared_hash = self.hash.ok_or(ChunkError::MissingHash)?;
        let actual_hash = sha256_hex(&self.buf);
        if declared_hash != actual_hash {
            return Err(ChunkError::HashMismatch {
                declared: declared_hash,
                actual: actual_hash,
            });
        }
        Ok(self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HARD_LIMIT: u64 = 8 * 1024 * 1024;

    fn reassemble_bytes(chunks: &[RemoteToolResponseChunk]) -> Result<Vec<u8>, ChunkError> {
        let mut r = ByteReassembler::new(HARD_LIMIT);
        for c in chunks {
            r.push(c)?;
        }
        r.finish()
    }

    /// Arbitrary bytes round-trip across a single chunk and across many.
    #[test]
    fn byte_chunks_round_trip_single_and_multi() {
        let payload = b"{\"status\":\"ok\",\"data\":\"hello world\"}".to_vec();
        let one = chunk_bytes("rt", &payload, 1024 * 1024);
        assert_eq!(one.len(), 1);
        assert!(one[0].last && one[0].sha256.is_some());
        assert_eq!(reassemble_bytes(&one).unwrap(), payload);

        let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let many = chunk_bytes("rt", &big, 256);
        assert!(many.len() > 1);
        for (i, c) in many.iter().enumerate() {
            assert_eq!(c.seq, i as u32);
            let is_last = i == many.len() - 1;
            assert_eq!(c.last, is_last);
            assert_eq!(c.sha256.is_some(), is_last);
        }
        assert_eq!(reassemble_bytes(&many).unwrap(), big);
    }

    /// An empty payload still produces one final chunk and round-trips to empty.
    #[test]
    fn byte_chunks_empty_payload() {
        let chunks = chunk_bytes("rt", b"", 1024);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].last);
        assert_eq!(reassemble_bytes(&chunks).unwrap(), Vec::<u8>::new());
    }

    /// A first chunk declaring a length above the hard limit is rejected before
    /// any buffer is grown to fit it.
    #[test]
    fn declared_len_over_hard_limit_rejected() {
        let mut r = ByteReassembler::new(16);
        let chunk = RemoteToolResponseChunk {
            request_id: "rt".into(),
            seq: 0,
            last: false,
            total_len: 1_000_000,
            payload_b64: BASE64.encode(b"AAAA"),
            sha256: None,
        };
        assert!(matches!(
            r.push(&chunk).unwrap_err(),
            ChunkError::TotalLenTooLarge { .. }
        ));
    }

    /// A non-final chunk carrying a sha256 is rejected.
    #[test]
    fn hash_on_non_final_rejected() {
        let mut r = ByteReassembler::new(HARD_LIMIT);
        let chunk = RemoteToolResponseChunk {
            request_id: "rt".into(),
            seq: 0,
            last: false,
            total_len: 100,
            payload_b64: BASE64.encode(b"AAAA"),
            sha256: Some("deadbeef".into()),
        };
        assert!(matches!(
            r.push(&chunk).unwrap_err(),
            ChunkError::HashOnNonFinal { .. }
        ));
    }

    /// A second chunk declaring a different total_len than the first is rejected.
    #[test]
    fn inconsistent_total_len_rejected() {
        let big: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let mut chunks = chunk_bytes("rt", &big, 256);
        let mut r = ByteReassembler::new(HARD_LIMIT);
        r.push(&chunks[0]).unwrap();
        chunks[1].total_len += 1;
        assert!(matches!(
            r.push(&chunks[1]).unwrap_err(),
            ChunkError::InconsistentTotalLen { .. }
        ));
    }

    /// A chunk arriving after the final chunk is rejected.
    #[test]
    fn chunk_after_final_rejected() {
        let payload = b"small".to_vec();
        let chunks = chunk_bytes("rt", &payload, 1024 * 1024);
        let mut r = ByteReassembler::new(HARD_LIMIT);
        r.push(&chunks[0]).unwrap();
        // A spurious extra chunk after the final one.
        let extra = RemoteToolResponseChunk {
            request_id: "rt".into(),
            seq: 1,
            last: false,
            total_len: payload.len() as u64,
            payload_b64: BASE64.encode(b"X"),
            sha256: None,
        };
        assert!(matches!(
            r.push(&extra).unwrap_err(),
            ChunkError::ChunkAfterFinal { .. }
        ));
    }

    /// A tampered final hash is caught.
    #[test]
    fn byte_tampered_hash_rejected() {
        let payload = b"some result bytes".to_vec();
        let mut chunks = chunk_bytes("rt", &payload, 1024 * 1024);
        chunks[0].sha256 = Some("deadbeef".into());
        assert!(matches!(
            reassemble_bytes(&chunks).unwrap_err(),
            ChunkError::HashMismatch { .. }
        ));
    }

    /// Out-of-order delivery is rejected.
    #[test]
    fn byte_out_of_order_rejected() {
        let big: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let chunks = chunk_bytes("rt", &big, 256);
        let mut r = ByteReassembler::new(HARD_LIMIT);
        r.push(&chunks[0]).unwrap();
        assert!(matches!(
            r.push(&chunks[2]).unwrap_err(),
            ChunkError::SeqGap { .. }
        ));
    }

    /// Same-length tampered payload bytes are caught by the hash.
    #[test]
    fn byte_tampered_payload_rejected() {
        let payload = b"some result bytes".to_vec();
        let mut chunks = chunk_bytes("rt", &payload, 1024 * 1024);
        chunks[0].payload_b64 = BASE64.encode(b"other result byte");
        assert!(matches!(
            reassemble_bytes(&chunks).unwrap_err(),
            ChunkError::HashMismatch { .. }
        ));
    }

    /// A stream without its final chunk is reported as incomplete.
    #[test]
    fn byte_missing_final_rejected() {
        let big: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let chunks = chunk_bytes("rt", &big, 256);
        let mut r = ByteReassembler::new(HARD_LIMIT);
        for c in &chunks[..chunks.len() - 1] {
            r.push(c).unwrap();
        }
        assert!(matches!(r.finish().unwrap_err(), ChunkError::MissingFinal));
    }

    /// No chunks at all is an error.
    #[test]
    fn byte_empty_rejected() {
        let r = ByteReassembler::new(HARD_LIMIT);
        assert!(matches!(r.finish().unwrap_err(), ChunkError::Empty));
    }
}
