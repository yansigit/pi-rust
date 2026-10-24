//! Frozen, dependency-free ABI-v4 prefix: deliberately never imports today's SDK.
use std::ffi::c_void;
#[repr(C)]
pub struct StringRef {
    data: *const u8,
    len: usize,
}
#[repr(C)]
pub struct StringValue {
    data: *mut u8,
    len: usize,
    capacity: usize,
}
#[repr(C)]
pub struct Prefix {
    abi_version: u32,
    struct_size: u32,
    free_string: extern "C" fn(StringValue),
    runtime_action: extern "C" fn(u32, StringRef, *mut StringValue, *mut c_void) -> i32,
    user_data: *mut c_void,
}
#[no_mangle]
pub unsafe extern "C" fn rpi_plugin_register(api: *const Prefix) -> i32 {
    if api.is_null() {
        return 2;
    }
    let api = unsafe { &*api };
    if api.abi_version != 4 || (api.struct_size as usize) < std::mem::size_of::<Prefix>() {
        return 1;
    }
    0
}
