use std::fmt::Write as _;

use super::{Timeline, TimelineFormat};

pub fn serialize(timeline: &Timeline, format: TimelineFormat) -> Result<String, serde_json::Error> {
    match format {
        TimelineFormat::Json => serde_json::to_string(timeline),
        TimelineFormat::Markdown => Ok(markdown(timeline)),
    }
}

#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    let (ascii, non_ascii) = text.chars().fold((0_usize, 0_usize), |counts, character| {
        if character.is_ascii() {
            (counts.0 + 1, counts.1)
        } else {
            (counts.0, counts.1 + 1)
        }
    });
    ((ascii as f64 / 4.0) + (non_ascii as f64 * 0.6)).ceil() as usize
}

fn markdown(timeline: &Timeline) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "# Zanei timeline");
    let _ = writeln!(
        output,
        "\nRange: {} — {}",
        timeline.range.since, timeline.range.until
    );
    let _ = writeln!(output, "Estimated tokens: {}", timeline.token_estimate);
    let _ = writeln!(
        output,
        "Truncated: {}",
        if timeline.truncated { "yes" } else { "no" }
    );
    for session in &timeline.sessions {
        let _ = writeln!(
            output,
            "\n## {} — {} · {}",
            session.start, session.end, session.app
        );
        if let Some(title) = &session.title_summary {
            let _ = writeln!(output, "\nTitle: {title}");
        }
        for activity in &session.activities {
            let _ = writeln!(output, "- {activity}");
        }
        if let Some(interactions) = &session.interactions {
            for interaction in interactions {
                let _ = writeln!(output, "  - [{}] {}", interaction.ts, interaction.activity);
            }
        }
        if session.content_snapshots > 0 {
            let _ = writeln!(output, "Content snapshots: {}", session.content_snapshots);
        }
    }
    output
}
