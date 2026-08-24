use super::*;

#[test]
fn chromium_activation_seeds_focus_without_a_focused_window_notification() {
    let mut api = FakeAxApi::chromium_profile();
    let (_lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    let (_click_sender, click_receiver) = click_channel();
    let (output_sender, _output_receiver) = sync_channel(1);
    let context = FocusContext::new();

    run_ax_loop(
        &mut api,
        &AtomicBool::new(true),
        &output_sender,
        &lifecycle_receiver,
        &click_receiver,
        capture_policy(),
        None,
        manual_accessibility_policy(),
        context.clone(),
        None,
        &AtomicU64::new(0),
        &AtomicU64::new(0),
        Arc::new(AtomicU64::new(0)),
    );

    let focus = context.current().expect("activation-time focus");
    assert_eq!(focus.app.bundle_id.as_deref(), Some("com.google.Chrome"));
    assert_eq!(focus.window.and_then(|window| window.id), Some(11));
}
