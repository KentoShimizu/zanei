use std::ops::Range;

use crate::config::RedactorKind;

pub(super) fn redact(value: &str, rule: RedactorKind, replacement: &str) -> String {
    let ranges = match rule {
        RedactorKind::Email => email_ranges(value),
        RedactorKind::CreditCard => credit_card_ranges(value),
        RedactorKind::Token => token_ranges(value),
    };
    replace_ranges(value, &ranges, replacement)
}

fn email_ranges(value: &str) -> Vec<Range<usize>> {
    let bytes = value.as_bytes();
    let mut ranges = Vec::new();

    for (at, byte) in bytes.iter().enumerate() {
        if *byte != b'@' {
            continue;
        }

        let mut start = at;
        while start > 0 && is_email_local_byte(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < bytes.len() && is_email_domain_byte(bytes[end]) {
            end += 1;
        }
        while end > at + 1 && bytes[end - 1] == b'.' {
            end -= 1;
        }

        let local = &value[start..at];
        let domain = &value[at + 1..end];
        if valid_email_local(local)
            && valid_email_domain(domain)
            && ranges
                .last()
                .is_none_or(|range: &Range<usize>| range.end <= start)
        {
            ranges.push(start..end);
        }
    }

    ranges
}

fn is_email_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_email_domain_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
}

fn valid_email_local(local: &str) -> bool {
    !local.is_empty() && !local.starts_with('.') && !local.ends_with('.') && !local.contains("..")
}

fn valid_email_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

fn credit_card_ranges(value: &str) -> Vec<Range<usize>> {
    let bytes = value.as_bytes();
    let mut ranges = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        if !bytes[cursor].is_ascii_digit() {
            cursor += 1;
            continue;
        }

        let start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_digit() || matches!(bytes[cursor], b' ' | b'-'))
        {
            cursor += 1;
        }
        let mut end = cursor;
        while end > start && matches!(bytes[end - 1], b' ' | b'-') {
            end -= 1;
        }

        let candidate = &value[start..end];
        let digits: Vec<u8> = candidate
            .bytes()
            .filter(u8::is_ascii_digit)
            .map(|byte| byte - b'0')
            .collect();
        if (13..=19).contains(&digits.len()) && passes_luhn(&digits) {
            ranges.push(start..end);
        }
    }

    ranges
}

fn passes_luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            let value = u32::from(*digit);
            if index % 2 == 1 {
                let doubled = value * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                value
            }
        })
        .sum();
    sum % 10 == 0
}

fn token_ranges(value: &str) -> Vec<Range<usize>> {
    const LABELS: &[&str] = &[
        "access_token",
        "authorization",
        "auth_token",
        "api_key",
        "api-key",
        "apikey",
        "secret",
        "token",
    ];
    const TOKEN_PREFIXES: &[&str] = &[
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "xoxs-",
        "ghp_",
        "sk-",
    ];

    let bytes = value.as_bytes();
    let mut ranges = Vec::new();
    for cursor in 0..bytes.len() {
        if !value.is_char_boundary(cursor) {
            continue;
        }

        if let Some(range) = LABELS
            .iter()
            .find_map(|label| token_after_label(value, cursor, label))
        {
            ranges.push(range);
        }
        if let Some(range) = token_after_bearer(value, cursor) {
            ranges.push(range);
        }
        if let Some(range) = TOKEN_PREFIXES
            .iter()
            .find_map(|prefix| prefixed_token(value, cursor, prefix))
        {
            ranges.push(range);
        }
        if let Some(range) = jwt_token(value, cursor) {
            ranges.push(range);
        }
    }

    normalize_ranges(ranges)
}

fn token_after_label(value: &str, start: usize, label: &str) -> Option<Range<usize>> {
    if !starts_with_ignore_ascii_case(&value[start..], label)
        || !word_boundary_before(value.as_bytes(), start)
    {
        return None;
    }

    let mut cursor = start + label.len();
    if !word_boundary_after(value.as_bytes(), cursor) {
        return None;
    }
    cursor = skip_ascii_spaces(value.as_bytes(), cursor);
    if !value
        .as_bytes()
        .get(cursor)
        .is_some_and(|byte| matches!(*byte, b':' | b'='))
    {
        return None;
    }
    cursor = skip_ascii_spaces(value.as_bytes(), cursor + 1);
    if starts_with_ignore_ascii_case(&value[cursor..], "bearer")
        && word_boundary_after(value.as_bytes(), cursor + "bearer".len())
    {
        cursor = skip_ascii_spaces(value.as_bytes(), cursor + "bearer".len());
    }

    scan_token(value, cursor, 4)
}

fn token_after_bearer(value: &str, start: usize) -> Option<Range<usize>> {
    const BEARER: &str = "bearer";
    if !starts_with_ignore_ascii_case(&value[start..], BEARER)
        || !word_boundary_before(value.as_bytes(), start)
        || !word_boundary_after(value.as_bytes(), start + BEARER.len())
    {
        return None;
    }
    let token_start = skip_ascii_spaces(value.as_bytes(), start + BEARER.len());
    if token_start == start + BEARER.len() {
        return None;
    }
    scan_token(value, token_start, 4)
}

fn prefixed_token(value: &str, start: usize, prefix: &str) -> Option<Range<usize>> {
    if !value[start..].starts_with(prefix) || !word_boundary_before(value.as_bytes(), start) {
        return None;
    }
    scan_token(value, start, 8)
}

fn jwt_token(value: &str, start: usize) -> Option<Range<usize>> {
    if !word_boundary_before(value.as_bytes(), start) {
        return None;
    }
    let range = scan_token(value, start, 20)?;
    let token = &value[range.clone()];
    let mut segments = token.split('.');
    let valid = (0..3).all(|_| {
        segments
            .next()
            .is_some_and(|segment| segment.len() >= 4 && segment.bytes().all(is_base64url_byte))
    });
    (valid && segments.next().is_none()).then_some(range)
}

fn scan_token(value: &str, start: usize, minimum_len: usize) -> Option<Range<usize>> {
    let bytes = value.as_bytes();
    let mut end = start;
    while end < bytes.len() && is_token_byte(bytes[end]) {
        end += 1;
    }
    while end > start && matches!(bytes[end - 1], b',' | b';' | b':') {
        end -= 1;
    }
    (end - start >= minimum_len).then_some(start..end)
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'~' | b'+' | b'/' | b'=')
}

fn is_base64url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'=')
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn word_boundary_before(bytes: &[u8], index: usize) -> bool {
    index == 0 || !is_identifier_byte(bytes[index - 1])
}

fn word_boundary_after(bytes: &[u8], index: usize) -> bool {
    index >= bytes.len() || !is_identifier_byte(bytes[index])
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn skip_ascii_spaces(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    cursor
}

fn normalize_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges
        .into_iter()
        .fold(Vec::new(), |mut normalized, range| {
            match normalized.last_mut() {
                Some(previous) if range.start <= previous.end => {
                    previous.end = previous.end.max(range.end);
                }
                _ => normalized.push(range),
            }
            normalized
        })
}

fn replace_ranges(value: &str, ranges: &[Range<usize>], replacement: &str) -> String {
    if ranges.is_empty() {
        return value.to_owned();
    }

    let removed_bytes: usize = ranges.iter().map(Range::len).sum();
    let mut redacted =
        String::with_capacity(value.len() - removed_bytes + replacement.len() * ranges.len());
    let mut cursor = 0;
    for range in ranges {
        redacted.push_str(&value[cursor..range.start]);
        redacted.push_str(replacement);
        cursor = range.end;
    }
    redacted.push_str(&value[cursor..]);
    redacted
}
