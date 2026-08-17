//! Rule-based timeline construction and serialization.

mod activity;
mod builder;
mod model;
mod render;

pub use builder::{MIN_TIMELINE_TOKEN_BUDGET_TOKENS, TimelineError, build};
pub use model::{
    Granularity, Interaction, Session, TimeRange, Timeline, TimelineFormat, TimelineOptions,
};
pub use render::{estimate_tokens, serialize};
