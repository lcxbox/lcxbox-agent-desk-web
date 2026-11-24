//! IPC file timestamps are signed 64-bit Unix nanoseconds.

use core::mem::MaybeUninit;
use jiff::Timestamp;
use wincode::config::ConfigCore;
use wincode::error::{ReadResult, WriteResult};
use wincode::io::{Reader, Writer};
use wincode::{SchemaRead, SchemaWrite, TypeMeta};

/// Zero-sized adapter for `Timestamp`. Implements
/// `SchemaWrite<C>` (Src = `Timestamp`) and `SchemaRead<'de, C>`
/// (Dst = `Timestamp`).
///
/// Encoded form: exactly an i64 (8 bytes under wincode's `FixInt +
/// LittleEndian` default configuration, which the daemon ↔ worker IPC
/// uses).
pub struct DateTimeLocalWincode;

// SAFETY: `size_of` always returns the exact 8 bytes that `write` emits
// (one i64). `zero_copy = false` because `Timestamp`'s in-memory
// representation is not an i64 — readers must materialise a timestamp
// rather than transmute.
unsafe impl<C: ConfigCore> SchemaWrite<C> for DateTimeLocalWincode {
    type Src = Timestamp;
    const TYPE_META: TypeMeta = TypeMeta::Static {
        size: 8,
        zero_copy: false,
    };

    fn size_of(_src: &Timestamp) -> WriteResult<usize> {
        Ok(8)
    }

    fn write(writer: impl Writer, src: &Timestamp) -> WriteResult<()> {
        let ns: i64 = i64::try_from(src.as_nanosecond()).unwrap_or(0);
        <i64 as SchemaWrite<C>>::write(writer, &ns)
    }
}

// SAFETY: `read` initialises `dst` only on the `Ok(())` path (after
// successfully reading the inner i64 and constructing the
// `Timestamp`). On error, `dst` is left untouched.
unsafe impl<'de, C: ConfigCore> SchemaRead<'de, C> for DateTimeLocalWincode {
    type Dst = Timestamp;
    const TYPE_META: TypeMeta = TypeMeta::Static {
        size: 8,
        zero_copy: false,
    };

    fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<Timestamp>) -> ReadResult<()> {
        let ns: i64 = <i64 as SchemaRead<'de, C>>::get(reader)?;
        dst.write(Timestamp::from_nanosecond(i128::from(ns)).expect("all i64 nanoseconds fit"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    /// Bare `Timestamp` field round-trips. A non-zero ns instant
    /// is chosen so any byte-order or sign-extension bug surfaces as a
    /// visible diff rather than a coincidental all-zero match.
    #[derive(Debug, Clone, PartialEq, Eq, wincode::SchemaWrite, wincode::SchemaRead)]
    struct BareDateTimeField {
        #[wincode(with = "DateTimeLocalWincode")]
        accessed: Timestamp,
    }

    /// `Option<Timestamp>` field — `#[wincode(with = "Option<...>")]`
    /// resolution against the bare adapter is the central finding
    /// that this module relies on, and it's re-verified here on the
    /// production crate so a future wincode-derive update that breaks
    /// the resolution shows up as a facade test failure.
    #[derive(Debug, Clone, PartialEq, Eq, wincode::SchemaWrite, wincode::SchemaRead)]
    struct OptionDateTimeField {
        #[wincode(with = "Option<DateTimeLocalWincode>")]
        deadline: Option<Timestamp>,
    }

    #[test]
    fn datetime_local_round_trips_wincode() {
        let value = BareDateTimeField {
            accessed: "2026-05-10T14:30:45Z".parse::<Timestamp>().unwrap(),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&value, config).expect("encode");
        // Bare DateTime field = 8 bytes (i64) flat.
        assert_eq!(bytes.len(), 8);
        let back: BareDateTimeField = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back, value);
    }

    #[test]
    fn option_datetime_local_round_trips_wincode_some_and_none() {
        let config = unbounded_config();

        let some_value = OptionDateTimeField {
            deadline: Some("2030-01-01T00:00:00Z".parse::<Timestamp>().unwrap()),
        };
        let bytes = wincode::config::serialize(&some_value, config).expect("encode some");
        let back: OptionDateTimeField =
            wincode::config::deserialize(&bytes, config).expect("decode some");
        assert_eq!(back, some_value);

        let none_value = OptionDateTimeField { deadline: None };
        let bytes = wincode::config::serialize(&none_value, config).expect("encode none");
        let back: OptionDateTimeField =
            wincode::config::deserialize(&bytes, config).expect("decode none");
        assert_eq!(back, none_value);
    }

    /// The Unix epoch (`ns = 0`) and a representative pre-epoch instant
    /// must both survive a round-trip — pre-epoch values produce
    /// negative `Unix nanoseconds` so this catches any sign-bit
    /// drop during the i64 conversion.
    #[test]
    fn datetime_local_round_trips_epoch_and_pre_epoch() {
        let config = unbounded_config();
        for value in [
            BareDateTimeField {
                accessed: Timestamp::UNIX_EPOCH,
            },
            BareDateTimeField {
                accessed: "1965-07-04T12:00:00Z".parse::<Timestamp>().unwrap(),
            },
        ] {
            let bytes = wincode::config::serialize(&value, config).expect("encode");
            let back: BareDateTimeField =
                wincode::config::deserialize(&bytes, config).expect("decode");
            assert_eq!(back, value);
        }
    }
}
