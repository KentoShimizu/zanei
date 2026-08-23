//! Safe ownership boundary around the macOS Accessibility C API.

mod accessibility;
mod cf;
mod element;
mod observer;
mod runtime;
mod snapshot;
mod types;
mod value_context;

pub use crate::ffi::geometry::{AxFrame, AxPoint, AxSize};
pub use crate::ffi::window_list::NativeWindow;
pub(crate) use accessibility::ManualAccessibilityPolicy;
pub(crate) use runtime::NativeAx;
pub use snapshot::{
    SnapshotAttribute, SnapshotAttributeResult, SnapshotAttributeValue, SnapshotAxApplication,
    SnapshotAxElement, SnapshotAxError,
};
pub use types::AxTextRange;
pub(crate) use types::{NativeAxError, NativeAxEvent, NativeElement, NativeHitTest};

use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError},
    },
    time::Instant,
};

use cf::{CfRef, OwnedCf, cf_string};
use time::OffsetDateTime;
use types::AX_ERROR_ATTRIBUTE_UNSUPPORTED;

const AX_ERROR_SUCCESS: i32 = 0;

struct ObserverContext {
    pid: i32,
    sender: SyncSender<QueuedNotification>,
    dropped: Arc<AtomicU64>,
}

#[derive(Clone, Copy)]
enum TargetKind {
    Window,
    Value,
}

struct QueuedNotification {
    pid: i32,
    element: OwnedCf,
    notification: OwnedCf,
    notification_at: Instant,
    observed_at: OffsetDateTime,
}

extern "C" fn observer_callback(
    _observer: CfRef,
    element: CfRef,
    notification: CfRef,
    context: *mut c_void,
) {
    let Some(context) = (unsafe { context.cast::<ObserverContext>().as_ref() }) else {
        crate::trace::trace!(
            "component=ax phase=callback action=drop pid=unknown reason=context_missing"
        );
        return;
    };
    let element = unsafe { OwnedCf::retain(element) };
    let notification = unsafe { OwnedCf::retain(notification) };
    let (Some(element), Some(notification)) = (element, notification) else {
        crate::trace::trace!(
            "component=ax phase=callback action=drop pid={} reason=retain_failed",
            context.pid
        );
        return;
    };
    match context.sender.try_send(QueuedNotification {
        pid: context.pid,
        element,
        notification,
        notification_at: Instant::now(),
        observed_at: OffsetDateTime::now_utc(),
    }) {
        Ok(()) => {
            crate::trace::trace!(
                "component=ax phase=callback action=enqueue pid={}",
                context.pid
            );
        }
        Err(TrySendError::Full(_)) => {
            crate::trace::trace!(
                "component=ax phase=callback action=drop pid={} reason=queue_full",
                context.pid
            );
            context.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {
            crate::trace::trace!(
                "component=ax phase=callback action=drop pid={} reason=queue_disconnected",
                context.pid
            );
            context.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn create_observer(pid: i32) -> Result<OwnedCf, NativeAxError> {
    let mut observer = ptr::null();
    let status = unsafe { AXObserverCreate(pid, observer_callback, &raw mut observer) };
    if status != AX_ERROR_SUCCESS {
        return Err(native_error("AXObserverCreate", status));
    }
    unsafe { OwnedCf::from_create(observer) }.ok_or_else(|| native_error("AXObserverCreate", -1))
}

fn add_notification(
    observer: CfRef,
    element: CfRef,
    notification: &str,
    context: *mut c_void,
) -> Result<(), NativeAxError> {
    let notification =
        cf_string(notification).ok_or_else(|| native_error("CFStringCreateWithCString", -1))?;
    let status =
        unsafe { AXObserverAddNotification(observer, element, notification.as_ptr(), context) };
    if status == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(native_error("AXObserverAddNotification", status))
    }
}

fn remove_notification(
    observer: CfRef,
    element: CfRef,
    notification: &str,
) -> Result<(), NativeAxError> {
    let notification =
        cf_string(notification).ok_or_else(|| native_error("CFStringCreateWithCString", -1))?;
    let status = unsafe { AXObserverRemoveNotification(observer, element, notification.as_ptr()) };
    if status == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(native_error("AXObserverRemoveNotification", status))
    }
}

const fn native_error(operation: &'static str, code: i32) -> NativeAxError {
    NativeAxError { operation, code }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXObserverCreate(
        pid: i32,
        callback: extern "C" fn(CfRef, CfRef, CfRef, *mut c_void),
        observer: *mut CfRef,
    ) -> i32;
    fn AXObserverAddNotification(
        observer: CfRef,
        element: CfRef,
        notification: CfRef,
        context: *mut c_void,
    ) -> i32;
    fn AXObserverRemoveNotification(observer: CfRef, element: CfRef, notification: CfRef) -> i32;
    fn AXObserverGetRunLoopSource(observer: CfRef) -> CfRef;
}

#[cfg(test)]
mod tests;
