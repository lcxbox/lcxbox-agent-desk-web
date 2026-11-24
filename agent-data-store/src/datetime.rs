//! UTC conversion at the SeaORM storage boundary.

use jiff::Timestamp;
use sea_orm::prelude::{DateTime, DateTimeUtc};

pub fn to_database(timestamp: Timestamp) -> DateTimeUtc {
    let nanos = timestamp.as_nanosecond();
    DateTimeUtc::from_timestamp(
        nanos.div_euclid(1_000_000_000) as i64,
        nanos.rem_euclid(1_000_000_000) as u32,
    )
    .expect("the database timestamp type covers the Jiff range")
}

pub fn from_database(value: DateTimeUtc) -> Result<Timestamp, jiff::Error> {
    Timestamp::from_nanosecond(
        i128::from(value.timestamp()) * 1_000_000_000 + i128::from(value.timestamp_subsec_nanos()),
    )
}

pub fn utc_now() -> DateTimeUtc {
    to_database(Timestamp::now())
}

pub fn naive_utc_now() -> DateTime {
    utc_now().naive_utc()
}

/// SQL date arithmetic uses the interval accepted by SeaORM's timestamp columns.
pub fn database_interval(
    duration: jiff::SignedDuration,
) -> sea_orm::sea_query::value::prelude::chrono::TimeDelta {
    let nanos = duration.as_nanos();
    sea_orm::sea_query::value::prelude::chrono::TimeDelta::new(
        nanos.div_euclid(1_000_000_000) as i64,
        nanos.rem_euclid(1_000_000_000) as u32,
    )
    .expect("interval exceeds the database range")
}

pub fn hour_bucket(value: DateTimeUtc) -> DateTimeUtc {
    DateTimeUtc::from_timestamp(value.timestamp().div_euclid(3600) * 3600, 0).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn database_conversion_preserves_pre_epoch_nanoseconds() {
        let value = to_database(Timestamp::new(0, -1).unwrap());
        assert_eq!(
            from_database(value).unwrap(),
            Timestamp::new(0, -1).unwrap()
        );
        assert_eq!(value.timestamp(), -1);
        assert_eq!(value.timestamp_subsec_nanos(), 999_999_999);
    }
}
