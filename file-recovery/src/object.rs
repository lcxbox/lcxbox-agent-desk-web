//! Content-addressed recovery objects: a fixed header followed by one zstd frame.
//!
//! A delta frame is compressed with its base's raw content as a referenced
//! prefix, an explicit window covering both inputs and long-distance matching,
//! the same arrangement as `zstd --patch-from`. Objects are named by the
//! SHA-256 of their raw content; the encoder that wrote them records the base
//! chain in the ledger, so readers never derive structure from object files.
use std::io;
use zstd::zstd_safe::{CCtx, CParameter, DCtx, DParameter};

const MAGIC: &[u8; 4] = b"LRDO";
const VERSION: u8 = 1;
pub(crate) const HEADER_BYTES: usize = 48;
const LEVEL: i32 = 3;
const MIN_WINDOW_LOG: u32 = 10;
const MAX_WINDOW_LOG: u32 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Full,
    Delta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub kind: Kind,
    pub raw_len: u64,
    pub base: Option<[u8; 32]>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn zstd_error(code: usize) -> io::Error {
    io::Error::other(zstd::zstd_safe::get_error_name(code))
}

/// `ceil(log2(max(base_len, raw_len)))`, clamped to zstd's supported range.
pub(crate) fn window_log(base_len: usize, raw_len: usize) -> u32 {
    let size = base_len.max(raw_len).max(1) as u64;
    let log = u64::BITS - (size - 1).leading_zeros();
    log.clamp(MIN_WINDOW_LOG, MAX_WINDOW_LOG)
}

fn header(kind: Kind, raw_len: usize, base: Option<&str>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES);
    bytes.extend_from_slice(MAGIC);
    bytes.push(VERSION);
    bytes.push(match kind {
        Kind::Full => 0,
        Kind::Delta => 1,
    });
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&(raw_len as u64).to_le_bytes());
    match base {
        Some(base) => bytes.extend_from_slice(&sha_bytes(base)?),
        None => bytes.extend_from_slice(&[0; 32]),
    }
    Ok(bytes)
}

pub(crate) fn sha_bytes(hex: &str) -> io::Result<[u8; 32]> {
    if !crate::valid_id(hex) {
        return Err(invalid("invalid object name"));
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| invalid("invalid object name"))?;
    }
    Ok(bytes)
}

pub(crate) fn parse_header(frame: &[u8]) -> io::Result<(Header, &[u8])> {
    if frame.len() < HEADER_BYTES || &frame[..4] != MAGIC || frame[4] != VERSION {
        return Err(invalid("unrecognized recovery object"));
    }
    let kind = match frame[5] {
        0 => Kind::Full,
        1 => Kind::Delta,
        _ => return Err(invalid("unrecognized recovery object kind")),
    };
    let raw_len = u64::from_le_bytes(frame[8..16].try_into().unwrap());
    let base: [u8; 32] = frame[16..48].try_into().unwrap();
    let base = match kind {
        Kind::Full => None,
        Kind::Delta => Some(base),
    };
    Ok((
        Header {
            kind,
            raw_len,
            base,
        },
        &frame[HEADER_BYTES..],
    ))
}

pub(crate) fn encode_full(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut frame = header(Kind::Full, raw.len(), None)?;
    let mut context = CCtx::create();
    context
        .set_parameter(CParameter::CompressionLevel(LEVEL))
        .map_err(zstd_error)?;
    compress_into(&mut context, raw, &mut frame)?;
    Ok(frame)
}

pub(crate) fn encode_delta(raw: &[u8], base_sha: &str, base: &[u8]) -> io::Result<Vec<u8>> {
    let mut frame = header(Kind::Delta, raw.len(), Some(base_sha))?;
    let mut context = CCtx::create();
    for parameter in [
        CParameter::CompressionLevel(LEVEL),
        CParameter::WindowLog(window_log(base.len(), raw.len())),
        CParameter::EnableLongDistanceMatching(true),
    ] {
        context.set_parameter(parameter).map_err(zstd_error)?;
    }
    context.ref_prefix(base).map_err(zstd_error)?;
    compress_into(&mut context, raw, &mut frame)?;
    Ok(frame)
}

fn compress_into(context: &mut CCtx<'_>, raw: &[u8], frame: &mut Vec<u8>) -> io::Result<()> {
    let mut compressed = Vec::with_capacity(zstd::zstd_safe::compress_bound(raw.len()));
    context
        .compress2(&mut compressed, raw)
        .map_err(zstd_error)?;
    frame.extend_from_slice(&compressed);
    Ok(())
}

/// Decode one frame. `base` is the recorded base name and its raw content;
/// a frame whose header names another base (an object republished with a
/// different chain) is rejected instead of being decoded against it.
pub(crate) fn decode(frame: &[u8], base: Option<(&str, &[u8])>, limit: u64) -> io::Result<Vec<u8>> {
    let (header, payload) = parse_header(frame)?;
    if header.raw_len > limit {
        return Err(invalid("recovery object exceeds its bound"));
    }
    let raw_len = header.raw_len as usize;
    let mut context = DCtx::create();
    match (header.kind, base) {
        (Kind::Full, None) => (),
        (Kind::Delta, Some((base_sha, base))) if header.base == Some(sha_bytes(base_sha)?) => {
            context
                .set_parameter(DParameter::WindowLogMax(window_log(base.len(), raw_len)))
                .map_err(zstd_error)?;
            context.ref_prefix(base).map_err(zstd_error)?;
        }
        _ => return Err(invalid("recovery object base does not match its chain")),
    }
    // One spare byte proves the frame does not hold more than its header states.
    let mut raw = Vec::with_capacity(raw_len + 1);
    context.decompress(&mut raw, payload).map_err(zstd_error)?;
    if raw.len() != raw_len {
        return Err(invalid("recovery object length differs from its header"));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let mut out = Vec::new();
        for line in 0..lines {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.extend_from_slice(
                format!("line {line:08} value {:016x} lorem ipsum dolor\n", state).as_bytes(),
            );
        }
        out
    }

    #[test]
    fn window_log_covers_both_inputs() {
        assert_eq!(window_log(0, 0), MIN_WINDOW_LOG);
        assert_eq!(window_log(1 << 20, 10), 20);
        assert_eq!(window_log((1 << 20) + 1, 10), 21);
        assert_eq!(window_log(10, 16 << 20), 24);
    }

    #[test]
    fn full_and_delta_frames_round_trip() {
        let base = text(2000, 1);
        let mut changed = base.clone();
        changed.splice(500..510, b"EDITED!!!!".iter().copied());
        let full = encode_full(&changed).unwrap();
        assert_eq!(decode(&full, None, u64::MAX).unwrap(), changed);
        let delta = encode_delta(&changed, &"a".repeat(64), &base).unwrap();
        assert_eq!(parse_header(&delta).unwrap().0.kind, Kind::Delta);
        let base_sha = "a".repeat(64);
        assert_eq!(
            decode(&delta, Some((&base_sha, &base)), u64::MAX).unwrap(),
            changed
        );
        assert!(delta.len() * 10 < full.len());
        assert!(decode(&delta, None, u64::MAX).is_err());
        assert!(decode(&delta, Some((&"c".repeat(64), &base)), u64::MAX).is_err());
        assert!(decode(&full, Some((&base_sha, &base)), u64::MAX).is_err());
        assert!(decode(&full, None, changed.len() as u64 - 1).is_err());
    }

    #[test]
    fn large_one_line_change_delta_is_below_five_percent_of_full() {
        for size in [8usize << 20, 16 << 20] {
            let mut base = text(size / 60, 7);
            base.truncate(size);
            let mut changed = base.clone();
            let middle = size / 2;
            changed[middle..middle + 12].copy_from_slice(b"one-line-fix");
            let full = encode_full(&changed).unwrap();
            let delta = encode_delta(&changed, &"b".repeat(64), &base).unwrap();
            println!(
                "size {} MiB: full {} bytes, delta {} bytes ({:.4}%)",
                size >> 20,
                full.len(),
                delta.len(),
                delta.len() as f64 * 100.0 / full.len() as f64
            );
            assert!(delta.len() * 20 < full.len());
            assert_eq!(
                decode(&delta, Some((&"b".repeat(64), &base)), size as u64).unwrap(),
                changed
            );
        }
    }

    #[test]
    fn malformed_headers_are_rejected() {
        let frame = encode_full(b"content").unwrap();
        assert!(parse_header(&frame[..HEADER_BYTES - 1]).is_err());
        let mut wrong = frame.clone();
        wrong[0] = b'X';
        assert!(parse_header(&wrong).is_err());
        let mut wrong = frame.clone();
        wrong[5] = 9;
        assert!(parse_header(&wrong).is_err());
        let mut wrong = frame;
        wrong[8] = 6;
        assert!(decode(&wrong, None, u64::MAX).is_err());
    }
}
