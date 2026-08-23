use std::time::{Duration, Instant};

use zanei_collector::Permission;

use crate::{
    content_snapshot::{SnapshotAxApplication, worker::test_live_scan},
    ffi::{
        window_list,
        workspace::{frontmost_application, running_applications},
    },
    permission::{PermissionStatus, permission_status},
};

const PROBE_APPS: [(&str, &str); 2] = [
    ("Google Chrome", "com.google.Chrome"),
    ("Claude", "com.anthropic.claudefordesktop"),
];
const FRAME_EDGE_TOLERANCE_POINTS: f64 = 1.0;
const WINDOW_LIST_COST_ITERATIONS: usize = 100;

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
    let Some(app) = frontmost_application() else {
        println!("SKIP: no frontmost regular application context is available");
        return;
    };
    let Some(window) = window_list::front_window(i64::from(app.pid)) else {
        println!("SKIP: the frontmost application has no CG window id");
        return;
    };
    let Some(expected_window_id) = window.id else {
        println!("SKIP: the frontmost application has no CG window id");
        return;
    };
    let pid = app.pid;
    let output = test_live_scan(pid, expected_window_id)
        .expect("walk frontmost AX tree")
        .expect("frontmost window changed during probe");
    println!(
        "app={} window_id={} nodes={} ax_calls={} elapsed_ms={} bytes={} complete={} cutoff={}",
        app.name,
        expected_window_id,
        output.nodes,
        output.ax_calls,
        output.elapsed.as_millis(),
        output.text.len(),
        output.complete,
        output.cutoff.map_or("none", |cutoff| cutoff.trace_name())
    );
}

#[test]
#[ignore = "requires author-granted Accessibility and running Chrome and Claude apps"]
fn live_ax_frame_matches_cg_bounds() {
    if !accessibility_is_granted() {
        return;
    }
    let applications = running_applications();
    for (name, bundle_id) in PROBE_APPS {
        let application = applications
            .iter()
            .find(|application| application.bundle_id.as_deref() == Some(bundle_id))
            .unwrap_or_else(|| panic!("{name} ({bundle_id}) is not running"));
        let ax_application = SnapshotAxApplication::new(application.pid).expect("create AX app");
        let window = ax_application
            .focused_window()
            .expect("read focused AX window")
            .unwrap_or_else(|| panic!("{name} has no focused AX window"));
        let frame = window
            .frame()
            .expect("read focused AX window frame")
            .unwrap_or_else(|| panic!("{name} has no AX window frame"));
        let window_id = window_list::window_id_for_frame(i64::from(application.pid), frame)
            .unwrap_or_else(|| panic!("{name} AX frame did not match a CG window"));
        let windows = window_list::on_screen_windows(i64::from(application.pid));
        let matched = windows
            .iter()
            .find(|window| window.id == window_id)
            .expect("resolved CG window remains on screen");
        let deltas = edge_deltas(frame, matched.bounds);
        assert!(
            deltas
                .iter()
                .all(|delta| *delta <= FRAME_EDGE_TOLERANCE_POINTS),
            "{name} frame edge delta exceeded tolerance: {deltas:?}"
        );
        println!(
            "app={name} pid={} window_id={window_id} ax=({:.1},{:.1},{:.1},{:.1}) cg=({:.1},{:.1},{:.1},{:.1}) edge_delta=({:.1},{:.1},{:.1},{:.1})",
            application.pid,
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
            matched.bounds.origin.x,
            matched.bounds.origin.y,
            matched.bounds.size.width,
            matched.bounds.size.height,
            deltas[0],
            deltas[1],
            deltas[2],
            deltas[3]
        );
    }
}

#[test]
#[ignore = "measures the native on-screen window-list read on this Mac"]
fn live_window_list_cost() {
    let pid = i64::from(std::process::id());
    let _ = window_list::on_screen_windows(pid);
    let mut samples = Vec::with_capacity(WINDOW_LIST_COST_ITERATIONS);
    for _ in 0..WINDOW_LIST_COST_ITERATIONS {
        let started = Instant::now();
        let _ = window_list::on_screen_windows(pid);
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    let total = samples.iter().sum::<Duration>();
    println!(
        "window_list_cost app=test_process iterations={} mean_us={} p50_us={} p95_us={} max_us={}",
        WINDOW_LIST_COST_ITERATIONS,
        total.as_micros() / WINDOW_LIST_COST_ITERATIONS as u128,
        samples[WINDOW_LIST_COST_ITERATIONS / 2].as_micros(),
        samples[WINDOW_LIST_COST_ITERATIONS * 95 / 100].as_micros(),
        samples[WINDOW_LIST_COST_ITERATIONS - 1].as_micros()
    );
}

fn accessibility_is_granted() -> bool {
    if matches!(
        permission_status(&Permission::Accessibility),
        Ok(PermissionStatus::Granted)
    ) {
        true
    } else {
        println!(
            "SKIP: AXIsProcessTrusted() returned false; grant Accessibility to the test binary"
        );
        false
    }
}

fn edge_deltas(
    frame: crate::ffi::geometry::AxFrame,
    bounds: crate::ffi::geometry::AxFrame,
) -> [f64; 4] {
    [
        (frame.origin.x - bounds.origin.x).abs(),
        (frame.origin.y - bounds.origin.y).abs(),
        (frame.origin.x + frame.size.width - bounds.origin.x - bounds.size.width).abs(),
        (frame.origin.y + frame.size.height - bounds.origin.y - bounds.size.height).abs(),
    ]
}
