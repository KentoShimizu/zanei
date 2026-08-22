//! Generic-password items in the user's login keychain (Security framework).
//!
//! Only the three operations the store key needs are wrapped: find, add, and
//! delete a single item identified by service and account. Every Core
//! Foundation object created here is released before the function returns.

use std::ffi::{CString, c_char, c_long, c_void};
use std::ptr::{self, NonNull};

type CfRef = *const c_void;
pub(crate) type OsStatus = i32;

pub(crate) const ERR_SEC_SUCCESS: OsStatus = 0;
pub(crate) const ERR_SEC_USER_CANCELED: OsStatus = -128;
pub(crate) const ERR_SEC_AUTH_FAILED: OsStatus = -25_293;
pub(crate) const ERR_SEC_DUPLICATE_ITEM: OsStatus = -25_299;
pub(crate) const ERR_SEC_ITEM_NOT_FOUND: OsStatus = -25_300;
pub(crate) const ERR_SEC_INTERACTION_NOT_ALLOWED: OsStatus = -25_308;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const SEC_UNLOCK_STATE_STATUS: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeychainFailure {
    /// A Core Foundation object could not be created (out of memory or an
    /// interior NUL in a string).
    Allocation,
    Status(OsStatus),
}

struct OwnedCf(NonNull<c_void>);

impl OwnedCf {
    /// Takes ownership of a +1 reference returned by a Core Foundation create function.
    unsafe fn from_create(value: CfRef) -> Option<Self> {
        NonNull::new(value.cast_mut()).map(Self)
    }

    fn as_ptr(&self) -> CfRef {
        self.0.as_ptr()
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        // SAFETY: `self` owns exactly one retain count obtained from a create function.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

fn cf_string(value: &str) -> Option<OwnedCf> {
    let value = CString::new(value).ok()?;
    // SAFETY: `value` is a valid NUL-terminated UTF-8 string that outlives the call.
    let string =
        unsafe { CFStringCreateWithCString(ptr::null(), value.as_ptr(), CF_STRING_ENCODING_UTF8) };
    // SAFETY: CFStringCreateWithCString follows the create rule.
    unsafe { OwnedCf::from_create(string) }
}

fn cf_data(bytes: &[u8]) -> Option<OwnedCf> {
    let length = isize::try_from(bytes.len()).ok()?;
    // SAFETY: `bytes` is valid for `length` bytes and is copied by CFDataCreate.
    let data = unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), length) };
    // SAFETY: CFDataCreate follows the create rule.
    unsafe { OwnedCf::from_create(data) }
}

fn cf_dictionary(keys: &[CfRef], values: &[CfRef]) -> Option<OwnedCf> {
    debug_assert_eq!(keys.len(), values.len());
    let count = c_long::try_from(keys.len()).ok()?;
    // SAFETY: both slices hold `count` valid Core Foundation objects that stay alive for the
    // duration of the call; the type callbacks retain them for the dictionary's lifetime.
    let dictionary = unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            count,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast::<c_void>(),
            (&raw const kCFTypeDictionaryValueCallBacks).cast::<c_void>(),
        )
    };
    // SAFETY: CFDictionaryCreate follows the create rule.
    unsafe { OwnedCf::from_create(dictionary) }
}

fn data_bytes(data: CfRef) -> Option<Vec<u8>> {
    // SAFETY: `data` is a non-null object returned by SecItemCopyMatching with kSecReturnData,
    // which is a CFData; the type check guards against an unexpected type anyway.
    unsafe {
        if data.is_null() || CFGetTypeID(data) != CFDataGetTypeID() {
            return None;
        }
        let length = usize::try_from(CFDataGetLength(data)).ok()?;
        let pointer = CFDataGetBytePtr(data);
        if pointer.is_null() {
            return Some(Vec::new());
        }
        Some(std::slice::from_raw_parts(pointer, length).to_vec())
    }
}

/// Reads the password data of the generic-password item for `service` / `account`.
pub(crate) fn find_generic_password(
    service: &str,
    account: &str,
) -> Result<Option<Vec<u8>>, KeychainFailure> {
    let service = cf_string(service).ok_or(KeychainFailure::Allocation)?;
    let account = cf_string(account).ok_or(KeychainFailure::Allocation)?;
    // SAFETY: the Security and Core Foundation constants are process-lifetime globals.
    let (keys, values) = unsafe {
        (
            [
                kSecClass,
                kSecAttrService,
                kSecAttrAccount,
                kSecReturnData,
                kSecMatchLimit,
            ],
            [
                kSecClassGenericPassword,
                service.as_ptr(),
                account.as_ptr(),
                kCFBooleanTrue,
                kSecMatchLimitOne,
            ],
        )
    };
    let query = cf_dictionary(&keys, &values).ok_or(KeychainFailure::Allocation)?;
    let mut result: CfRef = ptr::null();
    // SAFETY: `query` is a valid dictionary and `result` is writable storage for one object.
    let status = unsafe { SecItemCopyMatching(query.as_ptr(), &raw mut result) };
    match status {
        ERR_SEC_SUCCESS => {
            // SAFETY: on success the result is a +1 reference owned by the caller.
            let result = unsafe { OwnedCf::from_create(result) };
            let bytes = result.as_ref().and_then(|data| data_bytes(data.as_ptr()));
            Ok(Some(bytes.unwrap_or_default()))
        }
        ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        status => Err(KeychainFailure::Status(status)),
    }
}

/// Adds a generic-password item. Fails with `errSecDuplicateItem` when one exists.
pub(crate) fn add_generic_password(
    service: &str,
    account: &str,
    label: &str,
    description: &str,
    data: &[u8],
) -> Result<(), KeychainFailure> {
    let service = cf_string(service).ok_or(KeychainFailure::Allocation)?;
    let account = cf_string(account).ok_or(KeychainFailure::Allocation)?;
    let label = cf_string(label).ok_or(KeychainFailure::Allocation)?;
    let description = cf_string(description).ok_or(KeychainFailure::Allocation)?;
    let data = cf_data(data).ok_or(KeychainFailure::Allocation)?;
    // SAFETY: the Security constants are process-lifetime globals.
    let (keys, values) = unsafe {
        (
            [
                kSecClass,
                kSecAttrService,
                kSecAttrAccount,
                kSecAttrLabel,
                kSecAttrDescription,
                kSecValueData,
            ],
            [
                kSecClassGenericPassword,
                service.as_ptr(),
                account.as_ptr(),
                label.as_ptr(),
                description.as_ptr(),
                data.as_ptr(),
            ],
        )
    };
    let attributes = cf_dictionary(&keys, &values).ok_or(KeychainFailure::Allocation)?;
    // SAFETY: `attributes` is a valid dictionary; no result object is requested.
    let status = unsafe { SecItemAdd(attributes.as_ptr(), ptr::null_mut()) };
    if status == ERR_SEC_SUCCESS {
        Ok(())
    } else {
        Err(KeychainFailure::Status(status))
    }
}

/// Deletes the generic-password item. Returns `Ok(false)` when there was none.
pub(crate) fn delete_generic_password(
    service: &str,
    account: &str,
) -> Result<bool, KeychainFailure> {
    let service = cf_string(service).ok_or(KeychainFailure::Allocation)?;
    let account = cf_string(account).ok_or(KeychainFailure::Allocation)?;
    // SAFETY: the Security constants are process-lifetime globals.
    let (keys, values) = unsafe {
        (
            [kSecClass, kSecAttrService, kSecAttrAccount],
            [kSecClassGenericPassword, service.as_ptr(), account.as_ptr()],
        )
    };
    let query = cf_dictionary(&keys, &values).ok_or(KeychainFailure::Allocation)?;
    // SAFETY: `query` is a valid dictionary.
    let status = unsafe { SecItemDelete(query.as_ptr()) };
    match status {
        ERR_SEC_SUCCESS => Ok(true),
        ERR_SEC_ITEM_NOT_FOUND => Ok(false),
        status => Err(KeychainFailure::Status(status)),
    }
}

/// Enables or suppresses the keychain's own dialogs (unlock prompts, access
/// confirmations) for this process. With dialogs suppressed the calls above
/// fail with `errSecInteractionNotAllowed` instead of blocking on a window.
pub(crate) fn set_user_interaction_allowed(allowed: bool) -> OsStatus {
    // SAFETY: takes a plain Boolean and has no other preconditions.
    unsafe { SecKeychainSetUserInteractionAllowed(u8::from(allowed)) }
}

/// Whether the default (login) keychain is currently unlocked.
pub(crate) fn default_keychain_is_unlocked() -> Option<bool> {
    let mut status: u32 = 0;
    // SAFETY: a null keychain selects the default keychain; `status` is writable storage.
    let result = unsafe { SecKeychainGetStatus(ptr::null(), &raw mut status) };
    (result == ERR_SEC_SUCCESS).then_some(status & SEC_UNLOCK_STATE_STATUS != 0)
}

#[repr(C)]
struct CfDictionaryKeyCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
}

#[repr(C)]
struct CfDictionaryValueCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecClass: CfRef;
    static kSecClassGenericPassword: CfRef;
    static kSecAttrService: CfRef;
    static kSecAttrAccount: CfRef;
    static kSecAttrLabel: CfRef;
    static kSecAttrDescription: CfRef;
    static kSecValueData: CfRef;
    static kSecReturnData: CfRef;
    static kSecMatchLimit: CfRef;
    static kSecMatchLimitOne: CfRef;
    fn SecItemCopyMatching(query: CfRef, result: *mut CfRef) -> OsStatus;
    fn SecItemAdd(attributes: CfRef, result: *mut CfRef) -> OsStatus;
    fn SecItemDelete(query: CfRef) -> OsStatus;
    fn SecKeychainSetUserInteractionAllowed(allowed: u8) -> OsStatus;
    fn SecKeychainGetStatus(keychain: CfRef, status: *mut u32) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CfRef;
    static kCFTypeDictionaryKeyCallBacks: CfDictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: CfDictionaryValueCallBacks;
    fn CFRelease(value: CfRef);
    fn CFGetTypeID(value: CfRef) -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        value: *const c_char,
        encoding: u32,
    ) -> CfRef;
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> CfRef;
    fn CFDataGetLength(data: CfRef) -> isize;
    fn CFDataGetBytePtr(data: CfRef) -> *const u8;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const CfRef,
        values: *const CfRef,
        count: c_long,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfRef;
}
