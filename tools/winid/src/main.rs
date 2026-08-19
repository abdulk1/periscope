// Prints the on-screen window ids owned by a process, so `screencapture -l<id>`
// captures one window and nothing else on the display.
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionaryRef;
use core_foundation::number::CFNumberRef;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::window::{copy_window_info, kCGNullWindowID, kCGWindowListOptionOnScreenOnly};

unsafe extern "C" {
    fn CFDictionaryGetValue(
        dict: CFDictionaryRef,
        key: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFNumberGetValue(
        number: CFNumberRef,
        the_type: i32,
        value_ptr: *mut std::ffi::c_void,
    ) -> bool;
}

fn string_for(dict: CFDictionaryRef, key: &str) -> Option<String> {
    let key = CFString::new(key);
    let value = unsafe { CFDictionaryGetValue(dict, key.as_CFTypeRef() as _) };
    (!value.is_null())
        .then(|| unsafe { CFString::wrap_under_get_rule(value as CFStringRef) }.to_string())
}

fn number_for(dict: CFDictionaryRef, key: &str) -> Option<i64> {
    let key = CFString::new(key);
    let value = unsafe { CFDictionaryGetValue(dict, key.as_CFTypeRef() as _) };
    if value.is_null() {
        return None;
    }

    let mut out: i64 = 0;
    // 4 is kCFNumberSInt64Type.
    let ok = unsafe { CFNumberGetValue(value as CFNumberRef, 4, (&raw mut out).cast()) };
    ok.then_some(out)
}

fn main() {
    let wanted = std::env::args().nth(1).unwrap_or_else(|| "scope".to_string());
    let Some(list) = copy_window_info(kCGWindowListOptionOnScreenOnly, kCGNullWindowID) else {
        eprintln!("no window list");
        return;
    };

    for index in 0..list.len() {
        let Some(item) = list.get(index) else {
            continue;
        };
        let dict = *item as CFDictionaryRef;
        let owner = string_for(dict, "kCGWindowOwnerName").unwrap_or_default();
        if !owner.to_lowercase().contains(&wanted.to_lowercase()) {
            continue;
        }

        println!(
            "{} owner={owner} name={}",
            number_for(dict, "kCGWindowNumber").unwrap_or(-1),
            string_for(dict, "kCGWindowName").unwrap_or_default(),
        );
    }
}
