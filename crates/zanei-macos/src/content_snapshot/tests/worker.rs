use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::content_snapshot::{
    SnapshotAxError,
    tests::walker::FakeNode,
    worker::{SnapshotApplication, scan_application},
};

#[derive(Clone)]
struct FakeApplication {
    focused: Option<FakeNode>,
    windows: Vec<FakeNode>,
    windows_reads: Arc<AtomicUsize>,
}

impl SnapshotApplication for FakeApplication {
    type Window = FakeNode;

    fn pid(&self) -> i32 {
        7
    }

    fn focused_window(&self) -> Result<Option<Self::Window>, SnapshotAxError> {
        Ok(self.focused.clone())
    }

    fn windows(&self) -> Result<Vec<Self::Window>, SnapshotAxError> {
        self.windows_reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.windows.clone())
    }
}

fn application(focused_window_id: i64) -> FakeApplication {
    let focused = FakeNode::numbered_window(focused_window_id, "Focused");
    let other = FakeNode::numbered_window(22, "Previous");
    FakeApplication {
        focused: Some(focused.clone()),
        windows: vec![focused, other],
        windows_reads: Arc::new(AtomicUsize::new(0)),
    }
}

#[test]
fn non_focused_candidate_window_is_walked_by_window_id() {
    let app = application(11);
    let reads = Arc::clone(&app.windows_reads);
    let stop = AtomicBool::new(false);
    let output = scan_application(app, 22, &stop, |_, _| None)
        .expect("scan non-focused window")
        .expect("candidate window");

    assert_eq!(output.text, "Previous");
    assert_eq!(reads.load(Ordering::Relaxed), 1);

    let focused_output = scan_application(application(22), 22, &stop, |_, _| None)
        .expect("scan focused window")
        .expect("focused candidate");
    assert_eq!(
        output.ax_calls,
        focused_output.ax_calls + 3,
        "AXWindows and enumerated-window reads must be counted"
    );
}

#[test]
fn unknown_candidate_window_id_fails_closed() {
    let app = application(11);
    let reads = Arc::clone(&app.windows_reads);

    assert!(
        scan_application(app, 33, &AtomicBool::new(false), |_, _| None)
            .expect("scan missing window")
            .is_none()
    );
    assert_eq!(reads.load(Ordering::Relaxed), 1);
}
