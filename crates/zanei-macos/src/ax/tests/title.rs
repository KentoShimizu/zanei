use super::*;

#[test]
fn title_changes_include_the_previous_title() {
    let mut builder = builder();
    builder.add_app(app());
    let first = NativeWindow {
        title: Some("First".to_owned()),
        id: Some(1),
    };
    let second = NativeWindow {
        title: Some("Second".to_owned()),
        id: Some(1),
    };
    let _ = builder.event(NativeAxEvent::WindowFocused {
        pid: 7,
        window: first,
        observed_at: time::OffsetDateTime::UNIX_EPOCH,
    });
    let event = builder
        .event(NativeAxEvent::WindowTitleChanged {
            pid: 7,
            window: second,
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
        })
        .expect("window title event");

    let EventData::WindowTitle(data) = event.data else {
        panic!("expected a window.title payload");
    };
    assert_eq!(data.prev_title.as_deref(), Some("First"));
}
