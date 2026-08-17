//! Opt-in, content-free diagnostics for native capture decisions.

use std::{
    fmt,
    io::{self, Write},
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::focused_field::FieldClass;
use zanei_core::schema::FieldKind;

static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("ZANEI_TRACE").is_some_and(|value| value == "1"));

#[inline]
pub(crate) fn enabled() -> bool {
    *ENABLED
}

pub(crate) fn emit(arguments: fmt::Arguments<'_>) {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let _ = writeln!(
        io::stderr().lock(),
        "zanei_trace ts_ms={timestamp_ms} {arguments}"
    );
}

pub(crate) const fn field_class_name(field_class: FieldClass) -> &'static str {
    match field_class {
        FieldClass::SecureText => "secure_text",
        FieldClass::KnownText(FieldKind::Text) => "text",
        FieldClass::KnownText(FieldKind::Search) => "search",
        FieldClass::KnownText(FieldKind::Url) => "url",
        FieldClass::KnownText(FieldKind::Email) => "email",
        FieldClass::KnownText(FieldKind::Number) => "number",
        FieldClass::KnownText(FieldKind::Other) => "other_text",
        FieldClass::KnownSafeNonText => "safe_non_text",
        FieldClass::Unknown => "unknown",
    }
}

macro_rules! trace {
    ($($arguments:tt)*) => {{
        if $crate::trace::enabled() {
            $crate::trace::emit(format_args!($($arguments)*));
        }
    }};
}

pub(crate) use trace;
