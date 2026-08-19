// Prints the on-screen window ids owned by a process, so `screencapture -l<id>`
// captures one window and nothing else on the display.
//
// The workspace forbids `unsafe`; this tool cannot exist without it, because
// `CGWindowListCopyWindowInfo` returns an untyped CoreFoundation array. So the
// crate denies `unsafe_code` and exactly two functions opt back in — see
// `Cargo.toml`. Everything else is safe Rust, and every value taken out of a
// window dictionary has had its type id checked first. Reading a CFString as a
// CFNumber is not a wrong answer, it is whatever the bytes behind the pointer
// happen to be.
use std::ffi::c_void;

use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::{CFNumber, CFNumberRef};
use core_foundation::string::CFString;
use core_foundation_sys::base::Boolean;
use core_foundation_sys::number::{CFNumberType, kCFNumberSInt64Type};
use core_graphics::window::{copy_window_info, kCGNullWindowID, kCGWindowListOptionOnScreenOnly};

/// A window's dictionary, if the value really is one.
///
/// The array `copy_window_info` returns is untyped at the ABI, so the type id
/// decides rather than the documentation. Keys are `CFString`s and values are
/// left as `CFType` for the callers below to check individually.
#[expect(
    unsafe_code,
    reason = "CoreFoundation hands out untyped pointers; this is the only place that turns one into a Rust value"
)]
fn window_dictionary(item: *const c_void) -> Option<CFDictionary<CFString, CFType>> {
    if item.is_null() {
        return None;
    }

    // SAFETY: `item` is an element of the array returned by `copy_window_info`,
    // which owns it and outlives this call, so the get rule applies.
    let value = unsafe { CFType::wrap_under_get_rule(item) };
    value
        .instance_of::<CFDictionary<CFString, CFType>>()
        .then(|| {
            // SAFETY: the type id above says this is a CFDictionary, and
            // `CGWindowListCopyWindowInfo` documents CFString keys.
            unsafe { CFDictionary::wrap_under_get_rule(item as CFDictionaryRef) }
        })
}

/// The signed 64-bit value of a CFNumber.
///
/// `CFNumber::to_i64` would do this, but `core-foundation-sys` 0.8 declares
/// `CFNumberGetValue` as returning `bool`. The C return type is `Boolean`, an
/// `unsigned char`, and a Rust `bool` holding any byte other than 0 or 1 is
/// undefined behaviour — so the declaration is made here, correctly, rather
/// than borrowed.
#[expect(
    unsafe_code,
    reason = "reading a CFNumber out into a Rust integer is a raw framework call"
)]
fn to_i64(number: &CFNumber) -> Option<i64> {
    unsafe extern "C" {
        fn CFNumberGetValue(
            number: CFNumberRef,
            the_type: CFNumberType,
            value_ptr: *mut c_void,
        ) -> Boolean;
    }

    let mut out: i64 = 0;
    // SAFETY: `number` is a live CFNumber, and the destination is an `i64`,
    // which is what `kCFNumberSInt64Type` promises to write.
    let ok = unsafe {
        CFNumberGetValue(
            number.as_concrete_TypeRef(),
            kCFNumberSInt64Type,
            (&raw mut out).cast(),
        )
    };
    (ok != 0).then_some(out)
}

/// A string entry of a window dictionary, or `None` if it is absent or is
/// something else.
fn string_for(dictionary: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    let value = dictionary.find(CFString::new(key))?;
    Some(value.downcast::<CFString>()?.to_string())
}

/// A numeric entry of a window dictionary, or `None` if it is absent or is
/// something else.
fn number_for(dictionary: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
    let value = dictionary.find(CFString::new(key))?;
    to_i64(&value.downcast::<CFNumber>()?)
}

fn main() {
    let wanted = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "scope".to_string());
    let Some(list) = copy_window_info(kCGWindowListOptionOnScreenOnly, kCGNullWindowID) else {
        eprintln!("no window list");
        return;
    };

    for index in 0..list.len() {
        let Some(dictionary) = list.get(index).and_then(|item| window_dictionary(*item)) else {
            continue;
        };
        let owner = string_for(&dictionary, "kCGWindowOwnerName").unwrap_or_default();
        if !owner.to_lowercase().contains(&wanted.to_lowercase()) {
            continue;
        }

        println!(
            "{} owner={owner} name={}",
            number_for(&dictionary, "kCGWindowNumber").unwrap_or(-1),
            string_for(&dictionary, "kCGWindowName").unwrap_or_default(),
        );
    }
}
