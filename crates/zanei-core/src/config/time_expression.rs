use thiserror::Error;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Eq, Error, PartialEq)]
pub enum TimeExpressionError {
    #[error("time expression must not be empty")]
    Empty,
    #[error("relative time must be a positive integer followed by s, m, h, d, or w: {0}")]
    InvalidRelative(String),
    #[error("invalid RFC3339 timestamp: {input}: {source}")]
    InvalidTimestamp {
        input: String,
        #[source]
        source: time::error::Parse,
    },
    #[error("time expression is outside the supported range: {0}")]
    Overflow(String),
}

pub fn parse_time_expression(
    input: &str,
    now: OffsetDateTime,
) -> Result<OffsetDateTime, TimeExpressionError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(TimeExpressionError::Empty);
    }
    if input == "now" {
        return Ok(now);
    }
    if input.chars().last().is_some_and(is_relative_unit) {
        let duration = parse_duration_expression(input)?;
        return now
            .checked_sub(duration)
            .ok_or_else(|| TimeExpressionError::Overflow(input.to_owned()));
    }

    OffsetDateTime::parse(input, &Rfc3339).map_err(|source| TimeExpressionError::InvalidTimestamp {
        input: input.to_owned(),
        source,
    })
}

pub fn parse_duration_expression(input: &str) -> Result<Duration, TimeExpressionError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(TimeExpressionError::Empty);
    }

    let (amount, unit) = split_relative_expression(input)?;
    let seconds_per_unit = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        'w' => 7 * 24 * 60 * 60,
        _ => unreachable!("relative units are checked before this match"),
    };
    let seconds = amount
        .checked_mul(seconds_per_unit)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| TimeExpressionError::Overflow(input.to_owned()))?;
    Ok(Duration::seconds(seconds))
}

fn split_relative_expression(input: &str) -> Result<(u64, char), TimeExpressionError> {
    let Some(unit) = input.chars().last().filter(|unit| is_relative_unit(*unit)) else {
        return Err(TimeExpressionError::InvalidRelative(input.to_owned()));
    };
    let number = &input[..input.len() - unit.len_utf8()];
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TimeExpressionError::InvalidRelative(input.to_owned()));
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| TimeExpressionError::Overflow(input.to_owned()))?;
    if amount == 0 {
        return Err(TimeExpressionError::InvalidRelative(input.to_owned()));
    }
    Ok((amount, unit))
}

const fn is_relative_unit(unit: char) -> bool {
    matches!(unit, 's' | 'm' | 'h' | 'd' | 'w')
}
