use std::collections::{BTreeMap, BTreeSet};

use crate::schema::{Event, EventData, InputKeyKind};

const MAX_ACTIVITIES: usize = 5;

pub(super) fn summarize(events: &[Event], app: &str) -> Vec<String> {
    let mut activities = browser_activities(events);
    activities.extend(editing_activities(events));
    activities.extend(shortcut_activities(events));
    activities.extend(clipboard_activities(events));
    deduplicate(&mut activities);
    if activities.is_empty() {
        activities.push(format!("Used {app}"));
    }
    activities.truncate(MAX_ACTIVITIES);
    activities
}

pub(super) fn describe(event: &Event) -> String {
    match &event.data {
        EventData::BrowserNavigate(data) => data.tab_title.as_ref().map_or_else(
            || {
                data.url
                    .as_deref()
                    .map_or_else(|| "Viewed a page".to_owned(), |url| format!("Viewed {url}"))
            },
            |title| format!("Viewed \"{title}\""),
        ),
        EventData::InputKey(data) if data.kind == InputKeyKind::Shortcut => {
            data.combo.as_ref().map_or_else(
                || "Used a shortcut".to_owned(),
                |combo| format!("Used shortcut {combo}"),
            )
        }
        EventData::InputKey(data) => format!("Entered {} key events", data.count),
        EventData::InputScroll(data) => format!(
            "Scrolled {} {} times",
            scroll_direction(data.direction),
            data.count
        ),
        EventData::UiClick(_) => event
            .element
            .as_ref()
            .and_then(|element| element.title.as_ref())
            .map_or_else(
                || "Clicked a UI element".to_owned(),
                |title| format!("Clicked \"{title}\""),
            ),
        EventData::WindowTitle(_) => event
            .window
            .as_ref()
            .and_then(|window| window.title.as_ref())
            .map_or_else(
                || "Changed window title".to_owned(),
                |title| format!("Viewed \"{title}\""),
            ),
        _ => event.event_type.clone(),
    }
}

fn browser_activities(events: &[Event]) -> Vec<String> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < events.len() {
        let EventData::BrowserNavigate(first) = &events[index].data else {
            index += 1;
            continue;
        };
        let host = first.url.as_deref().and_then(url_host);
        let mut end = index + 1;
        while end < events.len() {
            let EventData::BrowserNavigate(next) = &events[end].data else {
                break;
            };
            if next.url.as_deref().and_then(url_host) != host {
                break;
            }
            end += 1;
        }
        let count = end - index;
        if count >= 2 {
            if let Some(host) = host {
                output.push(format!("Browsed {count} pages on {host}"));
            }
        } else {
            output.push(first.tab_title.as_ref().map_or_else(
                || {
                    first.url.as_deref().map_or_else(
                        || "Viewed a page".to_owned(),
                        |url| format!("Viewed \"{url}\""),
                    )
                },
                |title| format!("Viewed \"{title}\""),
            ));
        }
        index = end;
    }
    output
}

fn editing_activities(events: &[Event]) -> Vec<String> {
    let mut windows = BTreeSet::new();
    for event in events {
        let is_edit = matches!(&event.data, EventData::UiValue(_))
            || matches!(&event.data, EventData::InputKey(data) if data.kind == InputKeyKind::Text);
        if is_edit
            && let Some(title) = event
                .window
                .as_ref()
                .and_then(|window| window.title.as_ref())
        {
            windows.insert(title.clone());
        }
    }
    windows
        .into_iter()
        .map(|title| format!("Edited text in \"{title}\""))
        .collect()
}

fn shortcut_activities(events: &[Event]) -> Vec<String> {
    let mut counts = BTreeMap::<&str, u64>::new();
    for event in events {
        if let EventData::InputKey(data) = &event.data
            && data.kind == InputKeyKind::Shortcut
            && let Some(combo) = data.combo.as_deref()
            && let Some(verb) = shortcut_verb(combo)
        {
            *counts.entry(verb).or_default() += data.count;
        }
    }
    counts
        .into_iter()
        .map(|(verb, count)| format!("{verb} {count} times"))
        .collect()
}

fn clipboard_activities(events: &[Event]) -> Vec<String> {
    let count = events
        .iter()
        .filter(|event| {
            matches!(
                event.data,
                EventData::ClipboardCopy(_) | EventData::ClipboardPaste(_)
            )
        })
        .count();
    if count > 0 {
        vec![format!("Copied/pasted {count} times")]
    } else {
        Vec::new()
    }
}

fn shortcut_verb(combo: &str) -> Option<&'static str> {
    match combo.to_ascii_lowercase().as_str() {
        "cmd+s" => Some("Saved"),
        "cmd+c" => Some("Copied"),
        "cmd+v" => Some("Pasted"),
        "cmd+z" => Some("Undid"),
        _ => None,
    }
}

const fn scroll_direction(direction: crate::schema::ScrollDirection) -> &'static str {
    match direction {
        crate::schema::ScrollDirection::Up => "up",
        crate::schema::ScrollDirection::Down => "down",
        crate::schema::ScrollDirection::Left => "left",
        crate::schema::ScrollDirection::Right => "right",
    }
}

fn url_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, value)| value);
    let host = host_port
        .strip_prefix('[')
        .and_then(|value| value.split_once(']').map(|(host, _)| host))
        .or_else(|| host_port.split(':').next())?;
    (!host.is_empty()).then(|| host.trim_end_matches('.').to_ascii_lowercase())
}

fn deduplicate(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}
