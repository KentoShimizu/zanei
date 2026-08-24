use zanei_core::config::{FilterConfig, ScopedFilterConfig};

use super::*;

#[test]
fn filter_generation_reconciles_manual_accessibility_for_attached_apps() {
    let stop = Arc::new(AtomicBool::new(false));
    let policy = manual_accessibility_policy();
    let replacement = FilterConfig {
        text_content: ScopedFilterConfig {
            exclude_apps: vec!["dev.example.App".to_owned()],
            ..ScopedFilterConfig::default()
        },
        ..FilterConfig::default()
    };
    let mut api = FakeAxApi {
        running_applications: vec![app()],
        stop_after_poll: Some(Arc::clone(&stop)),
        stop_after_polls: Some(2),
        replacement_on_first_poll: Some((policy.clone(), replacement)),
        ..FakeAxApi::default()
    };
    let (_lifecycle_sender, lifecycle_receiver) = sync_channel(1);

    run_fake_ax_loop_with_policy(
        &mut api,
        stop.as_ref(),
        &lifecycle_receiver,
        &AtomicU64::new(0),
        Arc::new(AtomicU64::new(0)),
        policy,
    );

    assert_eq!(api.reconciled_manual_accessibility, vec![false]);
}
