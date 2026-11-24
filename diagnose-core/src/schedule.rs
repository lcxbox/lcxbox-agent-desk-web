//! Deterministic UTC rule validation and strictly-future occurrence calculation.

use desk_agent_protocol::schedule::{
    MAX_SCHEDULE_SPEC_BYTES, SCHEDULE_SCHEMA_VERSION, ScheduleRule, ScheduleSpec,
};
use jiff::{
    SignedDuration,
    civil::{DateTime, Time},
    fmt::temporal::Pieces,
};

/// Requirements must fit the existing conversation input admission limit.
pub const MAX_SCHEDULE_PROMPT_CHARS: usize = 16 * 1024;

pub const SCHEDULE_CALC_VERSION: &str = "utc-v1";
/// Initial server limits for every scheduled run; deployed policy is read from storage.
pub const DEFAULT_TASK_BUDGET: desk_agent_protocol::schedule::policy::TaskBudget =
    desk_agent_protocol::schedule::policy::TaskBudget {
        max_runs_per_utc_day: 24,
        max_calls_per_run: 64,
        max_model_tokens_per_run: 100_000,
        max_runtime_seconds: 900,
    };

pub const MIN_INTERVAL_SECONDS: u32 = 60;
pub const MAX_INTERVAL_SECONDS: u32 = 31_536_000;

pub mod definition;
pub mod dispatch;
pub mod due;
pub mod fresh_session;
pub mod history;
pub mod lifecycle;
pub mod model_admission;
pub mod model_usage;
pub mod permission_wait;
pub mod policy;
pub mod published_input;
pub mod retention;
pub mod review;
pub mod run_approval;
pub mod task_prompt;
pub mod timezone;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    InvalidJson,
    UnsupportedVersion,
    InvalidTime,
    InvalidWeekdays,
    InvalidInterval,
    OutOfRange,
    InvalidCreation,
}

pub fn normalize_creation(
    draft: &desk_agent_protocol::schedule::ScheduleCreation,
) -> Result<desk_agent_protocol::schedule::ScheduleCreation, ScheduleError> {
    use desk_agent_protocol::schedule::ScheduledTaskKind;
    let valid_id = |id: &str| {
        !id.is_empty() && id.len() <= 256 && id.trim() == id && !id.chars().any(char::is_control)
    };
    if !valid_id(&draft.client_create_key)
        || !valid_id(&draft.target_device_id)
        || draft.title.trim().is_empty()
        || draft.title.chars().count() > 240
        || draft.prompt.trim().is_empty()
        || draft.prompt.chars().count() > MAX_SCHEDULE_PROMPT_CHARS
        || draft
            .locale
            .as_ref()
            .is_some_and(|s| s.len() > 64 || s.chars().any(char::is_control))
        || draft.model_id.is_some_and(|id| id <= 0)
        || draft
            .source_conversation_id
            .as_deref()
            .is_some_and(|s| !valid_id(s))
    {
        return Err(ScheduleError::InvalidCreation);
    }
    match draft.kind {
        ScheduledTaskKind::ConversationResume
            if draft.source_conversation_id.is_none()
                || draft
                    .requirement_revision
                    .is_none_or(|r| r == 0 || r > i64::MAX as u64)
                || !matches!(
                    draft.spec.rule,
                    ScheduleRule::Once { .. } | ScheduleRule::AfterConfirmation { .. }
                ) =>
        {
            return Err(ScheduleError::InvalidCreation);
        }
        ScheduledTaskKind::FreshTask
            if draft.requirement_revision.is_some()
                || matches!(draft.spec.rule, ScheduleRule::AfterConfirmation { .. }) =>
        {
            return Err(ScheduleError::InvalidCreation);
        }
        _ => {}
    }
    let mut normalized = draft.clone();
    normalized.spec = normalize(&draft.spec)?;
    if let Some(confirmation) = &draft.time_confirmation {
        timezone::verify_confirmation(&normalized.spec, confirmation)
            .map_err(|_| ScheduleError::InvalidTime)?;
    }
    Ok(normalized)
}

// Civil UTC arithmetic covers the complete schedule range through 9999-12-31.
const EPOCH: DateTime = DateTime::constant(1970, 1, 1, 0, 0, 0, 0);

fn utc_at(ms: i64) -> Result<DateTime, ScheduleError> {
    EPOCH
        .checked_add(SignedDuration::from_millis(ms))
        .map_err(|_| ScheduleError::OutOfRange)
}

fn millis(utc: DateTime) -> i64 {
    EPOCH.duration_until(utc).as_millis() as i64
}

fn absolute(value: &str) -> Result<DateTime, ScheduleError> {
    // A schedule accepts a whole-second RFC 3339 instant with an explicit offset.
    let bytes = value.as_bytes();
    if !matches!(bytes.len(), 20 | 25)
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || &bytes[17..19] > b"59".as_slice()
        || !(bytes.len() == 20 && matches!(bytes[19], b'Z' | b'z')
            || bytes.len() == 25
                && matches!(bytes[19], b'+' | b'-')
                && bytes[22] == b':'
                && &bytes[20..22] <= b"23".as_slice())
    {
        return Err(ScheduleError::InvalidTime);
    }
    let parts = Pieces::parse(value).map_err(|_| ScheduleError::InvalidTime)?;
    let local = parts
        .time()
        .ok_or(ScheduleError::InvalidTime)?
        .to_datetime(parts.date());
    let offset = parts
        .to_numeric_offset()
        .ok_or(ScheduleError::InvalidTime)?;
    let instant = local
        .checked_sub(SignedDuration::from_secs(i64::from(offset.seconds())))
        .map_err(|_| ScheduleError::OutOfRange)?;
    if instant.year() < 1970 {
        return Err(ScheduleError::OutOfRange);
    }
    Ok(instant)
}

fn clock(value: &str) -> Result<Time, ScheduleError> {
    if value.len() != 8
        || !value.bytes().enumerate().all(|(i, b)| {
            if i == 2 || i == 5 {
                b == b':'
            } else {
                b.is_ascii_digit()
            }
        })
        || &value[6..8] > "59"
    {
        return Err(ScheduleError::InvalidTime);
    }
    value.parse().map_err(|_| ScheduleError::InvalidTime)
}

pub fn normalize(spec: &ScheduleSpec) -> Result<ScheduleSpec, ScheduleError> {
    if spec.schema_version != SCHEDULE_SCHEMA_VERSION {
        return Err(ScheduleError::UnsupportedVersion);
    }
    let mut spec = spec.clone();
    match &mut spec.rule {
        ScheduleRule::AfterConfirmation { delay_seconds } => {
            if !(1..=MAX_INTERVAL_SECONDS).contains(delay_seconds) {
                return Err(ScheduleError::InvalidInterval);
            }
        }
        ScheduleRule::Once { at } => {
            *at = format!("{}Z", absolute(at)?);
        }
        ScheduleRule::Interval {
            every_seconds,
            anchor_at,
        } => {
            if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(every_seconds) {
                return Err(ScheduleError::InvalidInterval);
            }
            *anchor_at = format!("{}Z", absolute(anchor_at)?);
        }
        ScheduleRule::Daily { utc_time } => {
            clock(utc_time)?;
        }
        ScheduleRule::Weekly { weekdays, utc_time } => {
            clock(utc_time)?;
            if weekdays.is_empty()
                || weekdays.len() > 7
                || weekdays.iter().any(|d| !(1..=7).contains(d))
            {
                return Err(ScheduleError::InvalidWeekdays);
            }
            weekdays.sort_unstable();
            if weekdays.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(ScheduleError::InvalidWeekdays);
            }
        }
    }
    Ok(spec)
}

pub fn parse_json(json: &str) -> Result<ScheduleSpec, ScheduleError> {
    if json.len() > MAX_SCHEDULE_SPEC_BYTES {
        return Err(ScheduleError::InvalidJson);
    }
    normalize(&serde_json::from_str(json).map_err(|_| ScheduleError::InvalidJson)?)
}

/// A bounded preview uses exactly the same iterator as production scheduling.
pub fn preview(
    spec: &ScheduleSpec,
    reference_ms: i64,
    count: usize,
) -> Result<Vec<i64>, ScheduleError> {
    if count > 5 {
        return Err(ScheduleError::OutOfRange);
    }
    normalize(spec)?;
    let mut cursor = reference_ms;
    let mut times = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(next) = next_after(spec, cursor)? else {
            break;
        };
        times.push(next);
        cursor = next;
    }
    Ok(times)
}

/// Publication cannot turn an elapsed one-shot intent into an immediate run.
pub fn validate_publication(
    spec: &ScheduleSpec,
    now_ms: i64,
    conversation_resume: bool,
) -> Result<ScheduleSpec, ScheduleError> {
    let mut normalized = normalize(spec)?;
    if let ScheduleRule::AfterConfirmation { delay_seconds } = normalized.rule {
        if !conversation_resume {
            return Err(ScheduleError::InvalidTime);
        }
        let at = now_ms
            .checked_add(i64::from(delay_seconds) * 1000)
            .and_then(|value| value.checked_add(999))
            .map(|value| value / 1000 * 1000)
            .and_then(|ms| utc_at(ms).ok())
            .ok_or(ScheduleError::OutOfRange)?;
        normalized.rule = ScheduleRule::Once {
            at: format!("{at}Z"),
        };
        normalized = normalize(&normalized)?;
    }
    if conversation_resume && !matches!(normalized.rule, ScheduleRule::Once { .. }) {
        return Err(ScheduleError::InvalidTime);
    }
    if next_after(&normalized, now_ms)?.is_none() {
        return Err(ScheduleError::InvalidTime);
    }
    Ok(normalized)
}

/// Returns a whole-second occurrence strictly after the millisecond reference.
pub fn next_after(spec: &ScheduleSpec, reference_ms: i64) -> Result<Option<i64>, ScheduleError> {
    let spec = normalize(spec)?;
    let reference_date = utc_at(reference_ms)?.date();
    if !(1970..=9999).contains(&reference_date.year()) {
        return Err(ScheduleError::OutOfRange);
    }
    let result = match &spec.rule {
        ScheduleRule::AfterConfirmation { .. } => return Err(ScheduleError::InvalidTime),
        ScheduleRule::Once { at } => {
            let at = millis(absolute(at)?);
            (at > reference_ms).then_some(at)
        }
        ScheduleRule::Interval {
            every_seconds,
            anchor_at,
        } => {
            let anchor = millis(absolute(anchor_at)?);
            let interval = i64::from(*every_seconds) * 1000;
            let next = if reference_ms < anchor {
                anchor
            } else {
                let n = (reference_ms - anchor) / interval + 1;
                anchor
                    .checked_add(n.checked_mul(interval).ok_or(ScheduleError::OutOfRange)?)
                    .ok_or(ScheduleError::OutOfRange)?
            };
            Some(next)
        }
        ScheduleRule::Daily { utc_time } | ScheduleRule::Weekly { utc_time, .. } => {
            let time = clock(utc_time)?;
            let mut result = None;
            for offset in 0..=7 {
                let day = reference_date
                    .checked_add(jiff::Span::new().days(offset))
                    .map_err(|_| ScheduleError::OutOfRange)?;
                if let ScheduleRule::Weekly { weekdays, .. } = &spec.rule
                    && !weekdays.contains(&(day.weekday().to_monday_one_offset() as u8))
                {
                    continue;
                }
                let at = millis(time.to_datetime(day));
                if at > reference_ms {
                    result = Some(at);
                    break;
                }
            }
            result
        }
    };
    if let Some(at) = result
        && utc_at(at).is_err()
    {
        return Err(ScheduleError::OutOfRange);
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

pub mod management_tools;
pub mod proposal;

#[cfg(test)]
mod confirmation_delay_tests {
    use super::*;
    #[test]
    fn delay_is_draft_only_and_resolves_from_each_confirmation_clock() {
        let draft = ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::AfterConfirmation { delay_seconds: 300 },
        };
        assert!(next_after(&draft, 1_000_000).is_err());
        assert!(validate_publication(&draft, 1_000_000, false).is_err());
        for now in [1_000_000, 9_000_123] {
            let active = validate_publication(&draft, now, true).unwrap();
            let at = next_after(&active, now).unwrap().unwrap();
            assert!(at >= now + 300_000 && at < now + 301_000);
            assert!(matches!(active.rule, ScheduleRule::Once { .. }));
        }
        for delay_seconds in [0, MAX_INTERVAL_SECONDS + 1] {
            assert!(
                normalize(&ScheduleSpec {
                    schema_version: 1,
                    rule: ScheduleRule::AfterConfirmation { delay_seconds }
                })
                .is_err()
            );
        }
    }
}

pub mod review_result;
