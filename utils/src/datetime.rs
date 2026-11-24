//! UTC timestamps at JSON and database boundaries.

use jiff::{Timestamp, fmt::temporal::DateTimePrinter, tz::Offset};

/// Use the established seconds/milliseconds/microseconds/nanoseconds wire precision.
pub fn rfc3339_at(timestamp: Timestamp, offset: Offset) -> String {
    let nanos = timestamp.subsec_nanosecond();
    let digits = if nanos == 0 {
        0
    } else if nanos % 1_000_000 == 0 {
        3
    } else if nanos % 1_000 == 0 {
        6
    } else {
        9
    };
    DateTimePrinter::new()
        .precision(Some(digits))
        .timestamp_with_offset_to_string(&timestamp, offset)
}

pub fn rfc3339(timestamp: Timestamp) -> String {
    rfc3339_at(timestamp, Offset::UTC)
}

/// Parse an RFC 3339 instant at a protocol boundary. The Temporal parser also
/// accepts basic ISO dates and bracketed timezone annotations, which the wire
/// protocol does not accept.
pub fn parse_rfc3339(value: &str) -> Result<Timestamp, jiff::Error> {
    let bytes = value.as_bytes();
    let offset_start = if matches!(bytes.last(), Some(b'Z' | b'z')) {
        bytes.len() - 1
    } else if bytes.len() >= 25
        && matches!(bytes[bytes.len() - 6], b'+' | b'-')
        && bytes[bytes.len() - 3] == b':'
        && &bytes[bytes.len() - 5..bytes.len() - 3] <= b"23".as_slice()
    {
        bytes.len() - 6
    } else {
        return Err(jiff::Error::from_args(format_args!(
            "expected an RFC 3339 offset"
        )));
    };
    if offset_start < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
        || (offset_start > 19
            && (bytes[19] != b'.'
                || offset_start == 20
                || !bytes[20..offset_start].iter().all(u8::is_ascii_digit)))
    {
        return Err(jiff::Error::from_args(format_args!(
            "expected an RFC 3339 timestamp"
        )));
    }
    let truncated;
    let text = if offset_start > 29 {
        truncated = format!("{}{}", &value[..29], &value[offset_start..]);
        &truncated
    } else {
        value
    };
    let timestamp: Timestamp = text.parse()?;
    if &bytes[17..19] == b"60" {
        timestamp.checked_add(jiff::SignedDuration::from_secs(1))
    } else {
        Ok(timestamp)
    }
}

/// Milliseconds since the epoch rounded down, including for pre-epoch instants.
pub fn unix_millis(timestamp: Timestamp) -> i64 {
    timestamp.as_nanosecond().div_euclid(1_000_000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_requires_wire_syntax_and_preserves_unix_precision() {
        for invalid in [
            "2025-01-01T00:00:00",
            "20250101T000000Z",
            "2025-01-01T00:00:00-24:00",
            "2025-01-01T00:00:00Z[UTC]",
            "Z",
        ] {
            assert!(parse_rfc3339(invalid).is_err(), "{invalid}");
        }
        let leap = parse_rfc3339("2016-12-31T23:59:60.125Z").unwrap();
        assert_eq!(leap, parse_rfc3339("2017-01-01T00:00:00.125Z").unwrap());
        assert_eq!(
            unix_millis(parse_rfc3339("1969-12-31 23:59:59.9999999999z").unwrap()),
            -1
        );
    }

    #[test]
    fn timestamp_wire_format_and_negative_milliseconds() {
        let ts = Timestamp::new(0, -1).unwrap();
        assert_eq!(unix_millis(ts), -1);
        assert_eq!(rfc3339(ts), "1969-12-31T23:59:59.999999999+00:00");
        assert_eq!(
            rfc3339(Timestamp::from_millisecond(100).unwrap()),
            "1970-01-01T00:00:00.100+00:00"
        );
    }
}
