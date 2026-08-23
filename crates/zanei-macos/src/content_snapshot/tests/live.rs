use zanei_collector::Permission;

use crate::{
    content_snapshot::worker::test_live_scan,
    ffi::eventtap::current_context,
    permission::{PermissionStatus, permission_status},
};

#[test]
#[ignore = "requires author-granted Accessibility and a manually selected frontmost window"]
fn live_frontmost_content_snapshot_metrics() {
    if !matches!(
        permission_status(&Permission::Accessibility),
        Ok(PermissionStatus::Granted)
    ) {
        println!(
            "SKIP: AXIsProcessTrusted() returned false; grant Accessibility to the test binary"
        );
        return;
    }
    let Some(context) = current_context() else {
        println!("SKIP: no frontmost regular application context is available");
        return;
    };
    let Some(expected_window_id) = context.window.as_ref().and_then(|window| window.id) else {
        println!("SKIP: the frontmost application has no CG window id");
        return;
    };
    let Ok(pid) = i32::try_from(context.app.pid) else {
        println!("SKIP: the frontmost application pid does not fit macOS AX pid_t");
        return;
    };
    let output = test_live_scan(pid, expected_window_id)
        .expect("walk frontmost AX tree")
        .expect("frontmost window changed during probe");
    println!(
        "app={} window_id={} nodes={} ax_calls={} elapsed_ms={} bytes={} complete={} cutoff={}",
        context.app.name,
        expected_window_id,
        output.nodes,
        output.ax_calls,
        output.elapsed.as_millis(),
        output.text.len(),
        output.complete,
        output.cutoff.map_or("none", |cutoff| cutoff.trace_name())
    );
}
