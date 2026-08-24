//! Carbon Secure Input call owned by `SecureInputMonitor`.

pub(crate) fn enabled() -> bool {
    // SAFETY: `SecureInputMonitor` owns the only production call site and invokes it on its
    // dedicated thread. Carbon documents this function as not thread-safe.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}
