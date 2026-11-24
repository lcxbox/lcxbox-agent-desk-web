//! IANA local-time editing resolves once to a fixed UTC rule.

use super::*;
use desk_agent_protocol::schedule::{
    LocalScheduleRule, ScheduleTimeConversion, ScheduleTimeConverted, ScheduleTimeFold,
};
use jiff::{
    civil::Date,
    tz::{AmbiguousOffset, Offset, TimeZone},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionError {
    InvalidTime,
    InvalidZone,
    NonexistentTime,
    AmbiguousTime {
        earlier_utc: String,
        later_utc: String,
    },
    InvalidRule(ScheduleError),
}

/// Resolve IANA names from the same versioned data on every host.
pub(crate) fn zone_name(name: &str) -> Option<&'static str> {
    jiff_tzdb::get(name)
        .map(|(canonical, _)| canonical)
        .filter(|canonical| *canonical == name)
}

pub(crate) fn zone(name: &str) -> Result<TimeZone, ConversionError> {
    let (canonical, data) = jiff_tzdb::get(name)
        .filter(|(canonical, _)| *canonical == name)
        .ok_or(ConversionError::InvalidZone)?;
    TimeZone::tzif(canonical, data).map_err(|_| ConversionError::InvalidZone)
}

pub fn convert(input: &ScheduleTimeConversion) -> Result<ScheduleTimeConverted, ConversionError> {
    if input.timezone != "UTC" && !input.timezone.contains('/') {
        return Err(ConversionError::InvalidZone);
    }
    let zone = zone(&input.timezone)?;
    if input.reference_date.len() != 10 {
        return Err(ConversionError::InvalidTime);
    }
    let date: Date = input
        .reference_date
        .parse()
        .map_err(|_| ConversionError::InvalidTime)?;
    if date.to_string() != input.reference_date || !(1970..=9999).contains(&date.year()) {
        return Err(ConversionError::InvalidTime);
    }
    let time = clock(&input.local_time).map_err(ConversionError::InvalidRule)?;
    let local = time.to_datetime(date);
    // Schedules cover the entire civil year 9999, including instants beyond Timestamp::MAX.
    let utc_at_offset = |offset: Offset| {
        local
            .checked_sub(SignedDuration::from_secs(i64::from(offset.seconds())))
            .map_err(|_| ConversionError::InvalidTime)
    };
    let offset = match zone.to_ambiguous_timestamp(local).offset() {
        AmbiguousOffset::Gap { .. } => return Err(ConversionError::NonexistentTime),
        AmbiguousOffset::Unambiguous { offset } => offset,
        AmbiguousOffset::Fold { before, after } => match input.fold {
            Some(ScheduleTimeFold::Earlier) => before,
            Some(ScheduleTimeFold::Later) => after,
            None => {
                return Err(ConversionError::AmbiguousTime {
                    earlier_utc: format!("{}Z", utc_at_offset(before)?),
                    later_utc: format!("{}Z", utc_at_offset(after)?),
                });
            }
        },
    };
    let offset_seconds = offset.seconds();
    let resolved = utc_at_offset(offset)?;
    let utc_time = resolved.time().to_string();
    let rule = match &input.rule {
        LocalScheduleRule::Once => ScheduleRule::Once {
            at: format!("{resolved}Z"),
        },
        LocalScheduleRule::Interval { every_seconds } => ScheduleRule::Interval {
            every_seconds: *every_seconds,
            anchor_at: format!("{resolved}Z"),
        },
        LocalScheduleRule::Daily => ScheduleRule::Daily { utc_time },
        LocalScheduleRule::Weekly { weekdays } => {
            // Validate before shifting so invalid or duplicate source days cannot disappear.
            normalize(&ScheduleSpec {
                schema_version: 1,
                rule: ScheduleRule::Weekly {
                    weekdays: weekdays.clone(),
                    utc_time: utc_time.clone(),
                },
            })
            .map_err(ConversionError::InvalidRule)?;
            let day_shift = ((i64::from(time.hour()) * 3600
                + i64::from(time.minute()) * 60
                + i64::from(time.second()))
                - i64::from(offset_seconds))
            .div_euclid(86_400);
            ScheduleRule::Weekly {
                weekdays: weekdays
                    .iter()
                    .map(|d| ((i64::from(*d) - 1 + day_shift).rem_euclid(7) + 1) as u8)
                    .collect(),
                utc_time,
            }
        }
    };
    let spec = normalize(&ScheduleSpec {
        schema_version: SCHEDULE_SCHEMA_VERSION,
        rule,
    })
    .map_err(ConversionError::InvalidRule)?;
    Ok(ScheduleTimeConverted {
        spec,
        offset_seconds,
        conversion_version: format!("utc-edit-v1/{}", jiff_tzdb::VERSION),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(zone: &str, date: &str, time: &str) -> ScheduleTimeConversion {
        ScheduleTimeConversion {
            timezone: zone.into(),
            reference_date: date.into(),
            local_time: time.into(),
            fold: None,
            rule: LocalScheduleRule::Daily,
        }
    }

    #[test]
    fn last_civil_day_remains_schedulable() {
        let mut edit = input("Asia/Tokyo", "9999-12-31", "23:59:59");
        edit.rule = LocalScheduleRule::Once;
        assert_eq!(
            convert(&edit).unwrap().spec.rule,
            ScheduleRule::Once {
                at: "9999-12-31T14:59:59Z".into()
            }
        );
    }

    #[test]
    fn conversion_fixes_utc_and_rotates_weekdays_across_midnight() {
        let mut edit = input("Asia/Shanghai", "2026-09-07", "01:00:00");
        edit.rule = LocalScheduleRule::Weekly {
            weekdays: vec![1, 3, 5],
        };
        let converted = convert(&edit).unwrap();
        assert_eq!(converted.offset_seconds, 28800);
        assert_eq!(
            converted.spec.rule,
            ScheduleRule::Weekly {
                weekdays: vec![2, 4, 7],
                utc_time: "17:00:00".into()
            }
        );
        assert!(
            !serde_json::to_string(&converted.spec)
                .unwrap()
                .contains("timezone")
        );
    }

    #[test]
    fn ambiguous_input_requires_explicit_selection_and_gap_is_rejected() {
        let mut edit = input("America/Los_Angeles", "2026-11-01", "01:30:00");
        assert!(matches!(
            convert(&edit),
            Err(ConversionError::AmbiguousTime { .. })
        ));
        edit.fold = Some(ScheduleTimeFold::Earlier);
        assert_eq!(convert(&edit).unwrap().offset_seconds, -25200);
        edit.fold = Some(ScheduleTimeFold::Later);
        assert_eq!(convert(&edit).unwrap().offset_seconds, -28800);
        assert_eq!(
            convert(&input("America/Los_Angeles", "2026-03-08", "02:30:00")),
            Err(ConversionError::NonexistentTime)
        );
    }

    #[test]
    fn quarter_hour_offsets_and_invalid_source_rules_are_handled() {
        let mut edit = input("Asia/Kathmandu", "2026-09-07", "14:00:00");
        assert_eq!(
            convert(&edit).unwrap().spec.rule,
            ScheduleRule::Daily {
                utc_time: "08:15:00".into()
            }
        );
        edit.rule = LocalScheduleRule::Weekly { weekdays: vec![0] };
        assert!(convert(&edit).is_err());
        assert_eq!(
            convert(&input("CST", "2026-09-07", "14:00:00")),
            Err(ConversionError::InvalidZone)
        );
    }
}

/// Recompute with the current tzdb; a stale confirmation cannot save a different rule.
pub fn verify_confirmation(
    spec: &ScheduleSpec,
    confirmation: &desk_agent_protocol::schedule::ScheduleTimeConfirmation,
) -> Result<(), ConversionError> {
    let current = convert(&confirmation.input)?;
    if current.conversion_version != confirmation.conversion_version
        || current.spec != normalize(spec).map_err(ConversionError::InvalidRule)?
    {
        return Err(ConversionError::InvalidTime);
    }
    Ok(())
}

pub fn upcoming_runs(spec: &ScheduleSpec, reference_ms: i64) -> Result<Vec<String>, ScheduleError> {
    preview(spec, reference_ms, 5)?
        .into_iter()
        .map(|at| utc_at(at).map(|time| format!("{time}Z")))
        .collect()
}

#[cfg(test)]
mod confirmation_tests {
    use super::*;
    use desk_agent_protocol::schedule::ScheduleTimeConfirmation;

    #[test]
    fn save_rejects_changed_conversion_version_input_or_utc_spec() {
        let input = ScheduleTimeConversion {
            timezone: "Asia/Shanghai".into(),
            reference_date: "2026-09-07".into(),
            local_time: "14:00:00".into(),
            fold: None,
            rule: LocalScheduleRule::Daily,
        };
        let converted = convert(&input).unwrap();
        let confirmation = ScheduleTimeConfirmation {
            input,
            conversion_version: converted.conversion_version.clone(),
        };
        verify_confirmation(&converted.spec, &confirmation).unwrap();
        let mut changed = confirmation.clone();
        changed.conversion_version.push_str("-stale");
        assert!(verify_confirmation(&converted.spec, &changed).is_err());
        changed = confirmation.clone();
        changed.input.local_time = "15:00:00".into();
        assert!(verify_confirmation(&converted.spec, &changed).is_err());
        let mut spec = converted.spec;
        spec.rule = ScheduleRule::Daily {
            utc_time: "07:00:00".into(),
        };
        assert!(verify_confirmation(&spec, &confirmation).is_err());
    }

    #[test]
    fn interval_conversion_and_preview_share_the_persisted_utc_iterator() {
        let input = ScheduleTimeConversion {
            timezone: "Asia/Kathmandu".into(),
            reference_date: "2026-09-07".into(),
            local_time: "14:00:00".into(),
            fold: None,
            rule: LocalScheduleRule::Interval {
                every_seconds: 3600,
            },
        };
        let converted = convert(&input).unwrap();
        assert_eq!(
            converted.spec.rule,
            ScheduleRule::Interval {
                anchor_at: "2026-09-07T08:15:00Z".into(),
                every_seconds: 3600
            }
        );
        let reference = "2026-09-07T08:14:59Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        let values = upcoming_runs(&converted.spec, reference).unwrap();
        assert_eq!(values.len(), 5);
        assert_eq!(values[0], "2026-09-07T08:15:00Z");
        assert_eq!(values[4], "2026-09-07T12:15:00Z");
    }
}
