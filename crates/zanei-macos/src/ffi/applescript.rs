//! In-process AppleScript execution with all Objective-C pointers kept private.

use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
};

#[cfg(target_arch = "x86_64")]
type ObjcBool = i8;
#[cfg(not(target_arch = "x86_64"))]
type ObjcBool = bool;

const FRONT_WINDOW_SCRIPT_TEMPLATE: &str = r#"
set chromeApp to path to application id "com.google.Chrome"
using terms from application "{application_path}"
with timeout of 1 second
    if application chromeApp is not running then return {"not_running"}
    tell application chromeApp
        if (count of windows) is 0 then return {"no_window"}

        set current_window to front window
        set current_mode to (mode of current_window) as text
        if current_mode is "incognito" then return {"incognito"}
        if current_mode is not "normal" then return {"unsupported_mode", current_mode}

        set current_tab to active tab of current_window
        return {"snapshot", (id of current_window) as text, name of current_window, (id of current_tab) as text, URL of current_tab, title of current_tab}
    end tell
end timeout
end using terms from
"#;

const TARGET_WINDOW_SCRIPT_TEMPLATE: &str = r#"
set chromeApp to path to application id "com.google.Chrome"
using terms from application "{application_path}"
with timeout of 1 second
    if application chromeApp is not running then return {"not_running"}
    tell application chromeApp
        if not (exists window id "{window_id}") then return {"no_window"}

        set current_window to window id "{window_id}"
        set current_mode to (mode of current_window) as text
        if current_mode is "incognito" then return {"incognito"}
        if current_mode is not "normal" then return {"unsupported_mode", current_mode}

        set current_tab to active tab of current_window
        return {"snapshot", (id of current_window) as text, name of current_window, (id of current_tab) as text, URL of current_tab, title of current_tab}
    end tell
end timeout
end using terms from
"#;

const SNAPSHOT_ITEM_COUNT: isize = 6;
const STATUS_ITEM_COUNT: isize = 1;
const UNSUPPORTED_MODE_ITEM_COUNT: isize = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) window_key: String,
    pub(crate) window_title: Option<String>,
    pub(crate) tab_key: String,
    pub(crate) url: String,
    pub(crate) tab_title: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    Snapshot(Snapshot),
    Incognito,
    NoWindow,
    NotRunning,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AppleScriptError {
    #[error("Objective-C class {0} is unavailable")]
    ClassUnavailable(&'static str),
    #[error("failed to allocate {0}")]
    Allocation(&'static str),
    #[error("Google Chrome is not installed")]
    ChromeUnavailable,
    #[error("AppleScript compilation failed (code {code:?})")]
    Compile { code: Option<i64> },
    #[error("AppleScript execution failed (code {code:?})")]
    Execute { code: Option<i64> },
    #[error("AppleScript returned an invalid Chrome response: {0}")]
    InvalidResponse(AppleScriptResponseError),
    #[error("Chrome returned an unsupported window mode")]
    UnsupportedMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum AppleScriptResponseError {
    #[error("empty descriptor list")]
    EmptyDescriptorList,
    #[error("unsupported-mode response has the wrong length")]
    UnsupportedModeLength,
    #[error("unknown response status")]
    UnknownStatus,
    #[error("status response has the wrong length")]
    StatusLength,
    #[error("snapshot response has the wrong length")]
    SnapshotLength,
    #[error("required descriptor item is not text")]
    RequiredItemNotText,
    #[error("string contains a NUL byte")]
    StringContainsNul,
}

pub(crate) struct AppleScriptClient {
    application_path: String,
    front_window_script: RetainedObject,
}

impl AppleScriptClient {
    pub(crate) fn new() -> Result<Self, AppleScriptError> {
        let _pool = AutoreleasePool::new()?;
        let application_path = chrome_application_path()?;
        let front_window_script = compile_script(&front_window_source(&application_path))?;
        Ok(Self {
            application_path,
            front_window_script,
        })
    }

    pub(crate) fn query(&mut self) -> Result<Observation, AppleScriptError> {
        let _pool = AutoreleasePool::new()?;
        execute_script(&self.front_window_script)
    }

    pub(crate) fn query_window(
        &mut self,
        window_id: &str,
    ) -> Result<Observation, AppleScriptError> {
        let _pool = AutoreleasePool::new()?;
        let script = compile_script(&target_window_source(&self.application_path, window_id))?;
        execute_script(&script)
    }
}

fn chrome_application_path() -> Result<String, AppleScriptError> {
    let class = class(c"NSWorkspace", "NSWorkspace")?;
    let workspace = send_object(class, c"sharedWorkspace");
    let bundle_id = autoreleased_string("com.google.Chrome")?;
    let url = send_object_with_object(
        workspace,
        c"URLForApplicationWithBundleIdentifier:",
        bundle_id,
    );
    let path = send_object(url, c"path");
    object_string(path).ok_or(AppleScriptError::ChromeUnavailable)
}

fn front_window_source(application_path: &str) -> String {
    substitute_application_path(FRONT_WINDOW_SCRIPT_TEMPLATE, application_path)
}

fn target_window_source(application_path: &str, window_id: &str) -> String {
    let escaped_window_id = escape_applescript_string(window_id);
    TARGET_WINDOW_SCRIPT_TEMPLATE
        .split("{window_id}")
        .map(|segment| substitute_application_path(segment, application_path))
        .collect::<Vec<_>>()
        .join(&escaped_window_id)
}

fn substitute_application_path(template: &str, application_path: &str) -> String {
    let escaped_path = escape_applescript_string(application_path);
    template.replace("{application_path}", &escaped_path)
}

fn escape_applescript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn compile_script(source: &str) -> Result<RetainedObject, AppleScriptError> {
    let source = autoreleased_string(source)?;
    let class = class(c"NSAppleScript", "NSAppleScript")?;
    let allocated = send_object(class, c"alloc");
    let script = send_object_with_object(allocated, c"initWithSource:", source);
    let script = RetainedObject::new(script, "NSAppleScript")?;
    let mut error_info = std::ptr::null_mut();
    if !send_bool_with_object_pointer(script.as_ptr(), c"compileAndReturnError:", &mut error_info) {
        return Err(AppleScriptError::Compile {
            code: error_code(error_info),
        });
    }
    Ok(script)
}

fn execute_script(script: &RetainedObject) -> Result<Observation, AppleScriptError> {
    let mut error_info = std::ptr::null_mut();
    let reply = send_object_with_object_pointer(
        script.as_ptr(),
        c"executeAndReturnError:",
        &mut error_info,
    );
    if reply.is_null() {
        return Err(AppleScriptError::Execute {
            code: error_code(error_info),
        });
    }
    parse_reply(reply)
}

fn parse_reply(reply: Object) -> Result<Observation, AppleScriptError> {
    let item_count = send_isize(reply, c"numberOfItems");
    if item_count < STATUS_ITEM_COUNT {
        return Err(AppleScriptError::InvalidResponse(
            AppleScriptResponseError::EmptyDescriptorList,
        ));
    }
    let status = required_item_string(reply, 1)?;
    match status.as_str() {
        "snapshot" => parse_snapshot(reply, item_count).map(Observation::Snapshot),
        "incognito" => parse_status(item_count, Observation::Incognito),
        "no_window" => parse_status(item_count, Observation::NoWindow),
        "not_running" => parse_status(item_count, Observation::NotRunning),
        "unsupported_mode" => {
            if item_count != UNSUPPORTED_MODE_ITEM_COUNT {
                return Err(AppleScriptError::InvalidResponse(
                    AppleScriptResponseError::UnsupportedModeLength,
                ));
            }
            let _ = required_item_string(reply, 2)?;
            Err(AppleScriptError::UnsupportedMode)
        }
        _ => Err(AppleScriptError::InvalidResponse(
            AppleScriptResponseError::UnknownStatus,
        )),
    }
}

fn parse_status(
    item_count: isize,
    observation: Observation,
) -> Result<Observation, AppleScriptError> {
    if item_count == STATUS_ITEM_COUNT {
        Ok(observation)
    } else {
        Err(AppleScriptError::InvalidResponse(
            AppleScriptResponseError::StatusLength,
        ))
    }
}

fn parse_snapshot(reply: Object, item_count: isize) -> Result<Snapshot, AppleScriptError> {
    if item_count != SNAPSHOT_ITEM_COUNT {
        return Err(AppleScriptError::InvalidResponse(
            AppleScriptResponseError::SnapshotLength,
        ));
    }
    Ok(Snapshot {
        window_key: required_item_string(reply, 2)?,
        window_title: item_string(reply, 3),
        tab_key: required_item_string(reply, 4)?,
        url: required_item_string(reply, 5)?,
        tab_title: item_string(reply, 6),
    })
}

fn required_item_string(reply: Object, index: isize) -> Result<String, AppleScriptError> {
    item_string(reply, index).ok_or(AppleScriptError::InvalidResponse(
        AppleScriptResponseError::RequiredItemNotText,
    ))
}

fn item_string(reply: Object, index: isize) -> Option<String> {
    let descriptor = send_object_with_isize(reply, c"descriptorAtIndex:", index);
    if descriptor.is_null() {
        return None;
    }
    let string = send_object(descriptor, c"stringValue");
    object_string(string)
}

fn error_code(error_info: Object) -> Option<i64> {
    if error_info.is_null() {
        return None;
    }
    let key = autoreleased_string("NSAppleScriptErrorNumber").ok()?;
    let number = send_object_with_object(error_info, c"objectForKey:", key);
    (!number.is_null()).then(|| send_i64(number, c"longLongValue"))
}

fn object_string(value: Object) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let bytes = send_c_string(value, c"UTF8String");
    if bytes.is_null() {
        return None;
    }
    // SAFETY: `UTF8String` returns a NUL-terminated pointer valid for the
    // lifetime of the NSString, which remains alive inside the autorelease pool.
    Some(
        unsafe { CStr::from_ptr(bytes) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn autoreleased_string(value: &str) -> Result<Object, AppleScriptError> {
    let value = std::ffi::CString::new(value).map_err(|_| {
        AppleScriptError::InvalidResponse(AppleScriptResponseError::StringContainsNul)
    })?;
    let class = class(c"NSString", "NSString")?;
    let string = send_object_with_c_string(class, c"stringWithUTF8String:", value.as_ptr());
    if string.is_null() {
        Err(AppleScriptError::Allocation("NSString"))
    } else {
        Ok(string)
    }
}

type Object = *mut c_void;
type Selector = *mut c_void;

struct RetainedObject(NonNull<c_void>);

impl RetainedObject {
    fn new(value: Object, name: &'static str) -> Result<Self, AppleScriptError> {
        NonNull::new(value)
            .map(Self)
            .ok_or(AppleScriptError::Allocation(name))
    }

    fn as_ptr(&self) -> Object {
        self.0.as_ptr()
    }
}

impl Drop for RetainedObject {
    fn drop(&mut self) {
        send_void(self.as_ptr(), c"release");
    }
}

struct AutoreleasePool(NonNull<c_void>);

impl AutoreleasePool {
    fn new() -> Result<Self, AppleScriptError> {
        let class = class(c"NSAutoreleasePool", "NSAutoreleasePool")?;
        let allocated = send_object(class, c"alloc");
        let pool = send_object(allocated, c"init");
        NonNull::new(pool)
            .map(Self)
            .ok_or(AppleScriptError::Allocation("NSAutoreleasePool"))
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        // `drain` also releases the pool, so it must not use RetainedObject's Drop.
        send_void(self.0.as_ptr(), c"drain");
    }
}

fn class(name: &CStr, display_name: &'static str) -> Result<Object, AppleScriptError> {
    // SAFETY: The class name is a valid, static C string.
    let value = unsafe { objc_get_class(name.as_ptr()) };
    if value.is_null() {
        Err(AppleScriptError::ClassUnavailable(display_name))
    } else {
        Ok(value)
    }
}

fn selector(name: &CStr) -> Selector {
    // SAFETY: Selector names are valid, static C strings.
    unsafe { sel_register_name(name.as_ptr()) }
}

macro_rules! send {
    ($signature:ty, $receiver:expr, $selector:expr $(, $argument:expr)*) => {{
        // SAFETY: Every call site supplies the Objective-C method's exact ABI.
        let function: $signature = unsafe { std::mem::transmute(objc_msg_send as *const ()) };
        unsafe { function($receiver, selector($selector) $(, $argument)*) }
    }};
}

fn send_object(receiver: Object, method: &CStr) -> Object {
    send!(
        unsafe extern "C" fn(Object, Selector) -> Object,
        receiver,
        method
    )
}

fn send_object_with_object(receiver: Object, method: &CStr, value: Object) -> Object {
    send!(
        unsafe extern "C" fn(Object, Selector, Object) -> Object,
        receiver,
        method,
        value
    )
}

fn send_object_with_object_pointer(receiver: Object, method: &CStr, value: *mut Object) -> Object {
    send!(
        unsafe extern "C" fn(Object, Selector, *mut Object) -> Object,
        receiver,
        method,
        value
    )
}

fn send_object_with_isize(receiver: Object, method: &CStr, value: isize) -> Object {
    send!(
        unsafe extern "C" fn(Object, Selector, isize) -> Object,
        receiver,
        method,
        value
    )
}

fn send_object_with_c_string(receiver: Object, method: &CStr, value: *const c_char) -> Object {
    send!(
        unsafe extern "C" fn(Object, Selector, *const c_char) -> Object,
        receiver,
        method,
        value
    )
}

fn send_bool_with_object_pointer(receiver: Object, method: &CStr, value: *mut Object) -> bool {
    send!(
        unsafe extern "C" fn(Object, Selector, *mut Object) -> ObjcBool,
        receiver,
        method,
        value
    ) as u8
        != 0
}

fn send_isize(receiver: Object, method: &CStr) -> isize {
    send!(
        unsafe extern "C" fn(Object, Selector) -> isize,
        receiver,
        method
    )
}

fn send_i64(receiver: Object, method: &CStr) -> i64 {
    send!(
        unsafe extern "C" fn(Object, Selector) -> i64,
        receiver,
        method
    )
}

fn send_c_string(receiver: Object, method: &CStr) -> *const c_char {
    send!(
        unsafe extern "C" fn(Object, Selector) -> *const c_char,
        receiver,
        method
    )
}

fn send_void(receiver: Object, method: &CStr) {
    send!(unsafe extern "C" fn(Object, Selector), receiver, method)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_window_source_leaves_frontmost_authority_to_focus_context() {
        let source = front_window_source("/Applications/Google Chrome.app");

        assert!(!source.contains("frontmost is false"));
        assert!(source.contains("set current_window to front window"));
    }

    #[test]
    fn targeted_source_reads_only_the_requested_window() {
        let source = target_window_source("/Applications/Google Chrome.app", "window-4321");

        assert!(source.contains("if not (exists window id \"window-4321\")"));
        assert!(source.contains("set current_window to window id \"window-4321\""));
        assert!(!source.contains("front window"));
    }

    #[test]
    fn targeted_source_escapes_opaque_window_identity_as_one_string_literal() {
        let window_id = "window-\\\" & return {\"private\"} & \"";

        let source = target_window_source("/Applications/Google Chrome.app", window_id);

        let escaped = "window-\\\\\\\" & return {\\\"private\\\"} & \\\"";
        assert_eq!(source.matches(escaped).count(), 2);
        assert!(!source.contains("window id \"window-\" & return"));
    }

    #[test]
    fn targeted_source_does_not_reinterpret_markers_in_values() {
        let application_path = "/Applications/{window_id}/Google Chrome.app";
        let window_id = "window-{application_path}";

        let source = target_window_source(application_path, window_id);

        assert_eq!(source.matches(application_path).count(), 1);
        assert_eq!(source.matches(window_id).count(), 2);
    }
}

#[link(name = "objc")]
unsafe extern "C" {
    #[link_name = "objc_getClass"]
    fn objc_get_class(name: *const c_char) -> Object;
    #[link_name = "sel_registerName"]
    fn sel_register_name(name: *const c_char) -> Selector;
    #[link_name = "objc_msgSend"]
    fn objc_msg_send();
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}
