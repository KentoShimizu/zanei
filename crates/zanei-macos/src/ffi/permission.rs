//! Native permission probes and Apple Event descriptor ownership.

use std::{
    ffi::{c_long, c_void},
    mem::MaybeUninit,
    ptr,
};

const TYPE_APPLICATION_BUNDLE_ID: u32 = u32::from_be_bytes(*b"bund");
const TYPE_WILDCARD: u32 = u32::from_be_bytes(*b"****");
const ASK_USER_IF_NEEDED_FALSE: u8 = 0;
const IO_HID_REQUEST_TYPE_LISTEN_EVENT: i32 = 1;

#[repr(C)]
struct OpaqueAeDataStorage {
    _private: [u8; 0],
}

type AeDataStorageType = *mut OpaqueAeDataStorage;
type AeDataStorage = *mut AeDataStorageType;

// Apple Event headers apply `#pragma pack(push, 2)` to AEDesc.
#[repr(C, packed(2))]
struct AeDesc {
    descriptor_type: u32,
    data_handle: AeDataStorage,
}

pub(crate) struct AutomationTarget {
    descriptor: AeDesc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutomationTargetError {
    BundleIdTooLong { byte_count: usize },
    CreateFailed { status: i16 },
}

impl AutomationTarget {
    pub(crate) fn new(bundle_id: &str) -> Result<Self, AutomationTargetError> {
        let byte_count = c_long::try_from(bundle_id.len()).map_err(|_| {
            AutomationTargetError::BundleIdTooLong {
                byte_count: bundle_id.len(),
            }
        })?;
        let mut descriptor = MaybeUninit::<AeDesc>::uninit();

        // SAFETY: `bundle_id` remains alive for the duration of the call, its byte length is
        // supplied explicitly, and `descriptor` points to writable storage for an AEDesc.
        let status = unsafe {
            ae_create_desc(
                TYPE_APPLICATION_BUNDLE_ID,
                bundle_id.as_ptr().cast::<c_void>(),
                byte_count,
                descriptor.as_mut_ptr(),
            )
        };
        if status != 0 {
            return Err(AutomationTargetError::CreateFailed { status });
        }

        // SAFETY: AECreateDesc returned success and therefore initialized the output descriptor.
        let descriptor = unsafe { descriptor.assume_init() };
        Ok(Self { descriptor })
    }

    pub(crate) fn permission_status(&self) -> i32 {
        // SAFETY: `self.descriptor` was initialized by AECreateDesc and remains owned by `self`.
        unsafe {
            ae_determine_permission_to_automate_target(
                &self.descriptor,
                TYPE_WILDCARD,
                TYPE_WILDCARD,
                ASK_USER_IF_NEEDED_FALSE,
            )
        }
    }
}

impl Drop for AutomationTarget {
    fn drop(&mut self) {
        // SAFETY: this descriptor was initialized by AECreateDesc, is exclusively owned, and is
        // disposed exactly once here. AEDisposeDesc accepts a valid descriptor in any state.
        let _ = unsafe { ae_dispose_desc(&mut self.descriptor) };
    }
}

pub(crate) fn accessibility_is_trusted() -> bool {
    // SAFETY: AXIsProcessTrusted has no parameters and returns a Core Foundation Boolean.
    unsafe { ax_is_process_trusted() != 0 }
}

pub(crate) fn request_accessibility() -> Option<bool> {
    // Both values are process-lifetime Core Foundation constants. A dictionary without retain
    // callbacks may therefore borrow them for the duration of this synchronous function call.
    let keys = [unsafe { k_ax_trusted_check_option_prompt }];
    let values = [unsafe { k_cf_boolean_true }];
    // SAFETY: the key/value arrays each contain one valid Core Foundation object pointer, the
    // count matches their lengths, and null callbacks are valid for borrowed process constants.
    let options = unsafe {
        cf_dictionary_create(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            ptr::null(),
            ptr::null(),
        )
    };
    if options.is_null() {
        return None;
    }
    // SAFETY: `options` is a valid CFDictionary created above. The API schedules prompting
    // asynchronously and only borrows the dictionary during this call.
    let trusted = unsafe { ax_is_process_trusted_with_options(options) != 0 };
    // SAFETY: `options` follows the Core Foundation create rule and is released exactly once.
    unsafe { cf_release(options) };
    Some(trusted)
}

pub(crate) fn input_monitoring_status() -> i32 {
    // SAFETY: the request value is the SDK-defined kIOHIDRequestTypeListenEvent enumerator.
    unsafe { io_hid_check_access(IO_HID_REQUEST_TYPE_LISTEN_EVENT) }
}

pub(crate) fn request_input_monitoring() -> bool {
    // SAFETY: the request value is the SDK-defined kIOHIDRequestTypeListenEvent enumerator.
    unsafe { io_hid_request_access(IO_HID_REQUEST_TYPE_LISTEN_EVENT) }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    #[link_name = "AXIsProcessTrusted"]
    fn ax_is_process_trusted() -> u8;

    #[link_name = "AXIsProcessTrustedWithOptions"]
    fn ax_is_process_trusted_with_options(options: *const c_void) -> u8;

    #[link_name = "kAXTrustedCheckOptionPrompt"]
    static k_ax_trusted_check_option_prompt: *const c_void;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    #[link_name = "IOHIDCheckAccess"]
    fn io_hid_check_access(request_type: i32) -> i32;

    #[link_name = "IOHIDRequestAccess"]
    fn io_hid_request_access(request_type: i32) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    #[link_name = "kCFBooleanTrue"]
    static k_cf_boolean_true: *const c_void;

    #[link_name = "CFDictionaryCreate"]
    fn cf_dictionary_create(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        value_count: c_long,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;

    #[link_name = "CFRelease"]
    fn cf_release(value: *const c_void);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    #[link_name = "AECreateDesc"]
    fn ae_create_desc(
        type_code: u32,
        data: *const c_void,
        data_size: c_long,
        result: *mut AeDesc,
    ) -> i16;

    #[link_name = "AEDeterminePermissionToAutomateTarget"]
    fn ae_determine_permission_to_automate_target(
        target: *const AeDesc,
        event_class: u32,
        event_id: u32,
        ask_user_if_needed: u8,
    ) -> i32;

    #[link_name = "AEDisposeDesc"]
    fn ae_dispose_desc(descriptor: *mut AeDesc) -> i16;
}

#[cfg(test)]
mod tests {
    use std::mem::{align_of, size_of};

    use super::AeDesc;

    #[test]
    fn ae_desc_matches_the_packed_sdk_layout() {
        assert_eq!(align_of::<AeDesc>(), 2);
        assert_eq!(size_of::<AeDesc>(), 12);
    }
}
