//! Objective-C `NSWorkspace` notification ownership and decoding.

use std::{
    ffi::{CStr, c_char, c_void},
    fmt,
    mem::{align_of, size_of, transmute},
    ptr::{self, NonNull},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    time::Duration,
};

const CALLBACK_QUEUE_CAPACITY: usize = 256;
const CONTEXT_IVAR: &CStr = c"_zaneiContext";

type ObjcId = *mut c_void;
type ObjcSel = *mut c_void;
#[cfg(target_arch = "x86_64")]
type ObjcBool = i8;
#[cfg(not(target_arch = "x86_64"))]
type ObjcBool = bool;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(isize)]
pub(crate) enum NativeApplicationActivationPolicy {
    Regular = 0,
    Accessory = 1,
    Prohibited = 2,
}

impl NativeApplicationActivationPolicy {
    fn from_raw(value: isize) -> Option<Self> {
        match value {
            0 => Some(Self::Regular),
            1 => Some(Self::Accessory),
            2 => Some(Self::Prohibited),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeApplication {
    pub(crate) name: String,
    pub(crate) bundle_id: Option<String>,
    pub(crate) pid: i32,
    pub(crate) activation_policy: NativeApplicationActivationPolicy,
}

#[derive(Debug)]
pub(crate) enum NativeWorkspaceEvent {
    Activated(NativeApplication),
    Launched(NativeApplication),
    Terminated(NativeApplication),
    DidWake,
}

#[derive(Debug)]
pub(crate) struct NativeWorkspaceError(&'static str);

impl fmt::Display for NativeWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

pub(crate) struct NativeWorkspaceObserver {
    notification_center: ObjcId,
    observer: OwnedObject,
    context: Box<CallbackContext>,
}

pub(crate) struct NativeWorkspaceEvents {
    receiver: Receiver<QueuedNotification>,
    dropped: Arc<AtomicU64>,
    enabled: Arc<AtomicBool>,
}

impl NativeWorkspaceObserver {
    pub(crate) fn new() -> Result<(Self, NativeWorkspaceEvents), NativeWorkspaceError> {
        let _pool = AutoreleasePool::new();
        let observer_class = observer_class()?;
        let observer =
            unsafe { OwnedObject::from_retained(send_id_0(observer_class, sel(c"new"))) }.ok_or(
                NativeWorkspaceError("failed to allocate workspace observer"),
            )?;
        let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let enabled = Arc::new(AtomicBool::new(false));
        let context = Box::new(CallbackContext {
            sender,
            dropped: Arc::clone(&dropped),
            enabled: Arc::clone(&enabled),
        });
        unsafe {
            set_context(observer.as_ptr(), (&raw const *context).cast_mut().cast());
        }

        let workspace_class = unsafe { class(c"NSWorkspace") };
        if workspace_class.is_null() {
            return Err(NativeWorkspaceError("NSWorkspace class is unavailable"));
        }
        let workspace = unsafe { send_id_0(workspace_class, sel(c"sharedWorkspace")) };
        let notification_center = unsafe { send_id_0(workspace, sel(c"notificationCenter")) };
        if notification_center.is_null() {
            return Err(NativeWorkspaceError(
                "NSWorkspace notification center is unavailable",
            ));
        }

        unsafe {
            add_observer(
                notification_center,
                observer.as_ptr(),
                c"zaneiActivated:",
                NSWorkspaceDidActivateApplicationNotification,
            );
            add_observer(
                notification_center,
                observer.as_ptr(),
                c"zaneiLaunched:",
                NSWorkspaceDidLaunchApplicationNotification,
            );
            add_observer(
                notification_center,
                observer.as_ptr(),
                c"zaneiTerminated:",
                NSWorkspaceDidTerminateApplicationNotification,
            );
            add_observer(
                notification_center,
                observer.as_ptr(),
                c"zaneiDidWake:",
                NSWorkspaceDidWakeNotification,
            );
        }

        Ok((
            Self {
                notification_center,
                observer,
                context,
            },
            NativeWorkspaceEvents {
                receiver,
                dropped,
                enabled,
            },
        ))
    }
}

impl NativeWorkspaceEvents {
    pub(crate) fn poll(&mut self, timeout: Duration) -> Vec<NativeWorkspaceEvent> {
        let _pool = AutoreleasePool::new();
        let first = match self.receiver.recv_timeout(timeout) {
            Ok(notification) => notification,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                return Vec::new();
            }
        };
        std::iter::once(first)
            .chain(self.receiver.try_iter())
            .filter_map(decode_notification)
            .collect()
    }

    pub(crate) fn enable(&mut self) {
        self.receiver.try_iter().for_each(drop);
        self.enabled.store(true, Ordering::Release);
    }

    pub(crate) fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    pub(crate) fn enabled_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.enabled)
    }

    pub(crate) fn frontmost_application(&self) -> Option<NativeApplication> {
        let _pool = AutoreleasePool::new();
        let workspace_class = unsafe { class(c"NSWorkspace") };
        let workspace = unsafe { send_id_0(workspace_class, sel(c"sharedWorkspace")) };
        let app = unsafe { send_id_0(workspace, sel(c"frontmostApplication")) };
        unsafe { decode_application(app) }
    }

    pub(crate) fn take_dropped_events(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

pub(crate) fn running_applications() -> Vec<NativeApplication> {
    let _pool = AutoreleasePool::new();
    let workspace_class = unsafe { class(c"NSWorkspace") };
    let workspace = unsafe { send_id_0(workspace_class, sel(c"sharedWorkspace")) };
    let apps = unsafe { send_id_0(workspace, sel(c"runningApplications")) };
    let count = unsafe { send_usize_0(apps, sel(c"count")) };
    (0..count)
        .filter_map(|index| {
            let app = unsafe { send_id_usize(apps, sel(c"objectAtIndex:"), index) };
            unsafe { decode_application(app) }
        })
        .collect()
}

impl Drop for NativeWorkspaceObserver {
    fn drop(&mut self) {
        let _pool = AutoreleasePool::new();
        self.context.enabled.store(false, Ordering::Release);
        unsafe {
            send_void_id(
                self.notification_center,
                sel(c"removeObserver:"),
                self.observer.as_ptr(),
            );
            set_context(self.observer.as_ptr(), ptr::null_mut());
        }
    }
}

struct CallbackContext {
    sender: SyncSender<QueuedNotification>,
    dropped: Arc<AtomicU64>,
    enabled: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum NotificationKind {
    Activated,
    Launched,
    Terminated,
    DidWake,
}

struct QueuedNotification {
    kind: NotificationKind,
    notification: OwnedObject,
}

unsafe impl Send for QueuedNotification {}

fn decode_notification(queued: QueuedNotification) -> Option<NativeWorkspaceEvent> {
    match queued.kind {
        NotificationKind::DidWake => Some(NativeWorkspaceEvent::DidWake),
        kind => {
            let notification = queued.notification.as_ptr();
            let user_info = unsafe { send_id_0(notification, sel(c"userInfo")) };
            let app =
                unsafe { send_id_id(user_info, sel(c"objectForKey:"), NSWorkspaceApplicationKey) };
            let app = unsafe { decode_application(app) }?;
            Some(match kind {
                NotificationKind::Activated => NativeWorkspaceEvent::Activated(app),
                NotificationKind::Launched => NativeWorkspaceEvent::Launched(app),
                NotificationKind::Terminated => NativeWorkspaceEvent::Terminated(app),
                NotificationKind::DidWake => unreachable!(),
            })
        }
    }
}

unsafe fn decode_application(app: ObjcId) -> Option<NativeApplication> {
    if app.is_null() {
        return None;
    }
    let name = unsafe { string_from_nsstring(send_id_0(app, sel(c"localizedName"))) }?;
    let bundle_id = unsafe { string_from_nsstring(send_id_0(app, sel(c"bundleIdentifier"))) };
    let pid = unsafe { send_i32_0(app, sel(c"processIdentifier")) };
    let activation_policy = NativeApplicationActivationPolicy::from_raw(unsafe {
        send_isize_0(app, sel(c"activationPolicy"))
    })?;
    (pid > 0).then_some(NativeApplication {
        name,
        bundle_id,
        pid,
        activation_policy,
    })
}

unsafe fn string_from_nsstring(string: ObjcId) -> Option<String> {
    if string.is_null() {
        return None;
    }
    let bytes = unsafe { send_c_string_0(string, sel(c"UTF8String")) };
    if bytes.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(bytes) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

extern "C" fn activated_callback(observer: ObjcId, _command: ObjcSel, notification: ObjcId) {
    enqueue_callback(observer, notification, NotificationKind::Activated);
}

extern "C" fn launched_callback(observer: ObjcId, _command: ObjcSel, notification: ObjcId) {
    enqueue_callback(observer, notification, NotificationKind::Launched);
}

extern "C" fn terminated_callback(observer: ObjcId, _command: ObjcSel, notification: ObjcId) {
    enqueue_callback(observer, notification, NotificationKind::Terminated);
}

extern "C" fn wake_callback(observer: ObjcId, _command: ObjcSel, notification: ObjcId) {
    enqueue_callback(observer, notification, NotificationKind::DidWake);
}

fn enqueue_callback(observer: ObjcId, notification: ObjcId, kind: NotificationKind) {
    let context = unsafe { get_context(observer).cast::<CallbackContext>().as_ref() };
    let Some(context) = context else {
        return;
    };
    if !context.enabled.load(Ordering::Acquire) {
        return;
    }
    let retained = unsafe { OwnedObject::retain(notification) };
    let Some(notification) = retained else {
        return;
    };
    if context
        .sender
        .try_send(QueuedNotification { kind, notification })
        .is_err()
    {
        context.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

fn observer_class() -> Result<ObjcId, NativeWorkspaceError> {
    static OBSERVER_CLASS: OnceLock<Result<usize, &'static str>> = OnceLock::new();
    OBSERVER_CLASS
        .get_or_init(|| unsafe { create_observer_class().map(|class| class as usize) })
        .as_ref()
        .copied()
        .map(|class| class as ObjcId)
        .map_err(|message| NativeWorkspaceError(message))
}

unsafe fn create_observer_class() -> Result<ObjcId, &'static str> {
    let existing = unsafe { class(c"ZaneiWorkspaceObserver") };
    if !existing.is_null() {
        return Ok(existing);
    }
    let superclass = unsafe { class(c"NSObject") };
    let observer_class =
        unsafe { objc_allocateClassPair(superclass, c"ZaneiWorkspaceObserver".as_ptr(), 0) };
    if observer_class.is_null() {
        return Err("failed to allocate Objective-C workspace observer class");
    }
    let pointer_alignment = align_of::<*mut c_void>().trailing_zeros() as u8;
    if unsafe {
        class_addIvar(
            observer_class,
            CONTEXT_IVAR.as_ptr(),
            size_of::<*mut c_void>(),
            pointer_alignment,
            c"^v".as_ptr(),
        )
    } as u8
        == 0
    {
        return Err("failed to add workspace observer context ivar");
    }
    for (name, callback) in [
        (c"zaneiActivated:", unsafe {
            transmute::<extern "C" fn(ObjcId, ObjcSel, ObjcId), unsafe extern "C" fn()>(
                activated_callback,
            )
        }),
        (c"zaneiLaunched:", unsafe {
            transmute::<extern "C" fn(ObjcId, ObjcSel, ObjcId), unsafe extern "C" fn()>(
                launched_callback,
            )
        }),
        (c"zaneiTerminated:", unsafe {
            transmute::<extern "C" fn(ObjcId, ObjcSel, ObjcId), unsafe extern "C" fn()>(
                terminated_callback,
            )
        }),
        (c"zaneiDidWake:", unsafe {
            transmute::<extern "C" fn(ObjcId, ObjcSel, ObjcId), unsafe extern "C" fn()>(
                wake_callback,
            )
        }),
    ] {
        if unsafe { class_addMethod(observer_class, sel(name), callback, c"v@:@".as_ptr()) } as u8
            == 0
        {
            return Err("failed to add workspace observer callback");
        }
    }
    unsafe { objc_registerClassPair(observer_class) };
    Ok(observer_class)
}

unsafe fn add_observer(center: ObjcId, observer: ObjcId, callback: &CStr, name: ObjcId) {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel, ObjcId, ObjcSel, ObjcId, ObjcId) =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe {
        function(
            center,
            sel(c"addObserver:selector:name:object:"),
            observer,
            sel(callback),
            name,
            ptr::null_mut(),
        );
    }
}

unsafe fn set_context(observer: ObjcId, context: ObjcId) {
    unsafe {
        object_setInstanceVariable(observer, CONTEXT_IVAR.as_ptr(), context);
    }
}

unsafe fn get_context(observer: ObjcId) -> ObjcId {
    let mut context = ptr::null_mut();
    unsafe {
        object_getInstanceVariable(observer, CONTEXT_IVAR.as_ptr(), &mut context);
    }
    context
}

struct OwnedObject(NonNull<c_void>);

impl OwnedObject {
    unsafe fn from_retained(object: ObjcId) -> Option<Self> {
        NonNull::new(object).map(Self)
    }

    unsafe fn retain(object: ObjcId) -> Option<Self> {
        NonNull::new(unsafe { objc_retain(object) }).map(Self)
    }

    fn as_ptr(&self) -> ObjcId {
        self.0.as_ptr()
    }
}

impl Drop for OwnedObject {
    fn drop(&mut self) {
        unsafe { objc_release(self.as_ptr()) };
    }
}

struct AutoreleasePool(ObjcId);

impl AutoreleasePool {
    fn new() -> Self {
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}

unsafe fn class(name: &CStr) -> ObjcId {
    unsafe { objc_getClass(name.as_ptr()) }
}

unsafe fn sel(name: &CStr) -> ObjcSel {
    unsafe { sel_registerName(name.as_ptr()) }
}

unsafe fn send_id_0(receiver: ObjcId, selector: ObjcSel) -> ObjcId {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel) -> ObjcId =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector) }
}

unsafe fn send_id_id(receiver: ObjcId, selector: ObjcSel, argument: ObjcId) -> ObjcId {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel, ObjcId) -> ObjcId =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector, argument) }
}

unsafe fn send_id_usize(receiver: ObjcId, selector: ObjcSel, argument: usize) -> ObjcId {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel, usize) -> ObjcId =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector, argument) }
}

unsafe fn send_void_id(receiver: ObjcId, selector: ObjcSel, argument: ObjcId) {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel, ObjcId) =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector, argument) };
}

unsafe fn send_usize_0(receiver: ObjcId, selector: ObjcSel) -> usize {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel) -> usize =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector) }
}

unsafe fn send_i32_0(receiver: ObjcId, selector: ObjcSel) -> i32 {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel) -> i32 =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector) }
}

unsafe fn send_isize_0(receiver: ObjcId, selector: ObjcSel) -> isize {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel) -> isize =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector) }
}

unsafe fn send_c_string_0(receiver: ObjcId, selector: ObjcSel) -> *const c_char {
    let function: unsafe extern "C" fn(ObjcId, ObjcSel) -> *const c_char =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector) }
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> ObjcId;
    fn sel_registerName(name: *const c_char) -> ObjcSel;
    fn objc_msgSend();
    fn objc_retain(object: ObjcId) -> ObjcId;
    fn objc_release(object: ObjcId);
    fn objc_autoreleasePoolPush() -> ObjcId;
    fn objc_autoreleasePoolPop(pool: ObjcId);
    fn objc_allocateClassPair(superclass: ObjcId, name: *const c_char, extra: usize) -> ObjcId;
    fn objc_registerClassPair(class: ObjcId);
    fn class_addIvar(
        class: ObjcId,
        name: *const c_char,
        size: usize,
        alignment: u8,
        types: *const c_char,
    ) -> ObjcBool;
    fn class_addMethod(
        class: ObjcId,
        selector: ObjcSel,
        implementation: unsafe extern "C" fn(),
        types: *const c_char,
    ) -> ObjcBool;
    fn object_setInstanceVariable(object: ObjcId, name: *const c_char, value: ObjcId) -> ObjcId;
    fn object_getInstanceVariable(
        object: ObjcId,
        name: *const c_char,
        value: *mut ObjcId,
    ) -> ObjcId;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSWorkspaceDidActivateApplicationNotification: ObjcId;
    static NSWorkspaceDidLaunchApplicationNotification: ObjcId;
    static NSWorkspaceDidTerminateApplicationNotification: ObjcId;
    static NSWorkspaceDidWakeNotification: ObjcId;
    static NSWorkspaceApplicationKey: ObjcId;
}
