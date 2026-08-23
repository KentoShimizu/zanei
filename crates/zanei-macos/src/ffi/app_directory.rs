//! Core Foundation and LaunchServices application metadata bindings.

use std::ffi::{CStr, c_char, c_void};
use std::fmt;
use std::mem::transmute;
use std::path::PathBuf;
use std::ptr;

const UTF8_ENCODING: u32 = 0x0800_0100;
const PROPERTY_LIST_IMMUTABLE: usize = 0;
const POSIX_PATH_STYLE: isize = 0;
const APPLICATION_NOT_FOUND_ERROR_CODE: isize = -10_814;

type CfObject = *const c_void;
type CfString = *const c_void;
type ObjcId = *mut c_void;
type ObjcSel = *mut c_void;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BundleMetadata {
    pub(crate) bundle_id: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) bundle_name: Option<String>,
}

#[derive(Debug)]
pub(crate) struct NativeAppDirectoryError(String);

impl NativeAppDirectoryError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for NativeAppDirectoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub(crate) fn parse_info_plist(bytes: &[u8]) -> Result<BundleMetadata, NativeAppDirectoryError> {
    let length = isize::try_from(bytes.len())
        .map_err(|_| NativeAppDirectoryError::new("property list is too large"))?;
    let data = unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), length) };
    let data = OwnedCf::new(data)
        .ok_or_else(|| NativeAppDirectoryError::new("failed to allocate property-list data"))?;
    let mut format = 0_isize;
    let mut error = ptr::null();
    let property_list = unsafe {
        CFPropertyListCreateWithData(
            ptr::null(),
            data.as_ptr(),
            PROPERTY_LIST_IMMUTABLE,
            &mut format,
            &mut error,
        )
    };
    let property_list = match OwnedCf::new(property_list) {
        Some(value) => value,
        None => {
            let reason = copy_error_description(error)
                .unwrap_or_else(|| "failed to parse property list".to_owned());
            if let Some(error) = OwnedCf::new(error) {
                drop(error);
            }
            return Err(NativeAppDirectoryError::new(reason));
        }
    };
    if unsafe { CFGetTypeID(property_list.as_ptr()) } != unsafe { CFDictionaryGetTypeID() } {
        return Err(NativeAppDirectoryError::new(
            "Info.plist root is not a dictionary",
        ));
    }

    Ok(BundleMetadata {
        bundle_id: dictionary_string(property_list.as_ptr(), c"CFBundleIdentifier")?,
        display_name: dictionary_string(property_list.as_ptr(), c"CFBundleDisplayName")?,
        bundle_name: dictionary_string(property_list.as_ptr(), c"CFBundleName")?,
    })
}

pub(crate) fn home_directory() -> Result<PathBuf, NativeAppDirectoryError> {
    let value = unsafe { NSHomeDirectory() };
    if value.is_null() {
        return Err(NativeAppDirectoryError::new(
            "Foundation did not return a home directory",
        ));
    }
    cf_string(value).map(PathBuf::from)
}

pub(crate) fn ensure_workspace_available() -> Result<(), NativeAppDirectoryError> {
    let workspace_class = unsafe { objc_getClass(c"NSWorkspace".as_ptr()) };
    if workspace_class.is_null() {
        return Err(NativeAppDirectoryError::new(
            "NSWorkspace class is unavailable",
        ));
    }
    let workspace = unsafe { send_id_0(workspace_class, c"sharedWorkspace") };
    if workspace.is_null() {
        return Err(NativeAppDirectoryError::new(
            "NSWorkspace shared instance is unavailable",
        ));
    }
    let applications = unsafe { send_id_0(workspace, c"runningApplications") };
    if applications.is_null() {
        return Err(NativeAppDirectoryError::new(
            "NSWorkspace running applications are unavailable",
        ));
    }
    Ok(())
}

pub(crate) fn application_path_for_bundle_id(
    bundle_id: &str,
) -> Result<Option<PathBuf>, NativeAppDirectoryError> {
    let bundle_id = create_cf_string(bundle_id)?;
    let mut error = ptr::null();
    let urls = unsafe { LSCopyApplicationURLsForBundleIdentifier(bundle_id.as_ptr(), &mut error) };
    let urls = match OwnedCf::new(urls) {
        Some(urls) => {
            if let Some(error) = OwnedCf::new(error) {
                drop(error);
            }
            urls
        }
        None => {
            let error = OwnedCf::new(error).ok_or_else(|| {
                NativeAppDirectoryError::new(
                    "LaunchServices application lookup returned neither URLs nor an error",
                )
            })?;
            if unsafe { CFErrorGetCode(error.as_ptr()) } == APPLICATION_NOT_FOUND_ERROR_CODE {
                return Ok(None);
            }
            let reason = copy_error_description(error.as_ptr()).unwrap_or_else(|| {
                "LaunchServices application lookup failed without a description".to_owned()
            });
            return Err(NativeAppDirectoryError::new(reason));
        }
    };
    let count = unsafe { CFArrayGetCount(urls.as_ptr()) };
    if count == 0 {
        return Ok(None);
    }
    let url = unsafe { CFArrayGetValueAtIndex(urls.as_ptr(), 0) };
    if url.is_null() {
        return Err(NativeAppDirectoryError::new(
            "LaunchServices returned an invalid application URL",
        ));
    }
    let path = unsafe { CFURLCopyFileSystemPath(url, POSIX_PATH_STYLE) };
    let path = OwnedCf::new(path).ok_or_else(|| {
        NativeAppDirectoryError::new("LaunchServices application URL has no filesystem path")
    })?;
    cf_string(path.as_ptr()).map(PathBuf::from).map(Some)
}

fn dictionary_string(
    dictionary: CfObject,
    key: &CStr,
) -> Result<Option<String>, NativeAppDirectoryError> {
    let key_name = key.to_string_lossy();
    let native_key = unsafe { CFStringCreateWithCString(ptr::null(), key.as_ptr(), UTF8_ENCODING) };
    let native_key = OwnedCf::new(native_key)
        .ok_or_else(|| NativeAppDirectoryError::new("failed to allocate property-list key"))?;
    let value = unsafe { CFDictionaryGetValue(dictionary, native_key.as_ptr()) };
    if value.is_null() {
        return Ok(None);
    }
    if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
        return Err(NativeAppDirectoryError::new(format!(
            "{key_name} is not a string"
        )));
    }
    cf_string(value).map(Some)
}

fn create_cf_string(value: &str) -> Result<OwnedCf, NativeAppDirectoryError> {
    let length = isize::try_from(value.len())
        .map_err(|_| NativeAppDirectoryError::new("string is too large"))?;
    let string =
        unsafe { CFStringCreateWithBytes(ptr::null(), value.as_ptr(), length, UTF8_ENCODING, 0) };
    OwnedCf::new(string)
        .ok_or_else(|| NativeAppDirectoryError::new("failed to allocate Core Foundation string"))
}

fn cf_string(value: CfString) -> Result<String, NativeAppDirectoryError> {
    let length = unsafe { CFStringGetLength(value) };
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, UTF8_ENCODING) }
        .checked_add(1)
        .ok_or_else(|| NativeAppDirectoryError::new("Core Foundation string is too large"))?;
    let capacity = usize::try_from(capacity)
        .map_err(|_| NativeAppDirectoryError::new("Core Foundation string is too large"))?;
    let mut bytes = vec![0_u8; capacity];
    let copied = unsafe {
        CFStringGetCString(
            value,
            bytes.as_mut_ptr().cast::<c_char>(),
            isize::try_from(capacity)
                .map_err(|_| NativeAppDirectoryError::new("string buffer is too large"))?,
            UTF8_ENCODING,
        )
    };
    if copied == 0 {
        return Err(NativeAppDirectoryError::new(
            "failed to decode Core Foundation string as UTF-8",
        ));
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8(bytes[..end].to_vec())
        .map_err(|error| NativeAppDirectoryError::new(format!("invalid UTF-8 string: {error}")))
}

fn copy_error_description(error: CfObject) -> Option<String> {
    if error.is_null() {
        return None;
    }
    let description = unsafe { CFErrorCopyDescription(error) };
    let description = OwnedCf::new(description)?;
    cf_string(description.as_ptr()).ok()
}

unsafe fn send_id_0(receiver: ObjcId, selector: &CStr) -> ObjcId {
    let selector = unsafe { sel_registerName(selector.as_ptr()) };
    let function: unsafe extern "C" fn(ObjcId, ObjcSel) -> ObjcId =
        unsafe { transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { function(receiver, selector) }
}

struct OwnedCf(CfObject);

impl OwnedCf {
    fn new(value: CfObject) -> Option<Self> {
        if value.is_null() {
            None
        } else {
            Some(Self(value))
        }
    }

    const fn as_ptr(&self) -> CfObject {
        self.0
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CfObject) -> isize;
    fn CFArrayGetValueAtIndex(array: CfObject, index: isize) -> CfObject;
    fn CFDataCreate(allocator: CfObject, bytes: *const u8, length: isize) -> CfObject;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dictionary: CfObject, key: CfObject) -> CfObject;
    fn CFErrorCopyDescription(error: CfObject) -> CfString;
    fn CFErrorGetCode(error: CfObject) -> isize;
    fn CFGetTypeID(value: CfObject) -> usize;
    fn CFPropertyListCreateWithData(
        allocator: CfObject,
        data: CfObject,
        options: usize,
        format: *mut isize,
        error: *mut CfObject,
    ) -> CfObject;
    fn CFRelease(value: CfObject);
    fn CFStringCreateWithBytes(
        allocator: CfObject,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external_representation: u8,
    ) -> CfString;
    fn CFStringCreateWithCString(
        allocator: CfObject,
        value: *const c_char,
        encoding: u32,
    ) -> CfString;
    fn CFStringGetCString(
        string: CfString,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
    fn CFStringGetLength(string: CfString) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetTypeID() -> usize;
    fn CFURLCopyFileSystemPath(url: CfObject, path_style: isize) -> CfString;
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    fn NSHomeDirectory() -> CfString;
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSCopyApplicationURLsForBundleIdentifier(
        bundle_id: CfString,
        out_error: *mut CfObject,
    ) -> CfObject;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> ObjcId;
    fn objc_msgSend();
    fn sel_registerName(name: *const c_char) -> ObjcSel;
}
