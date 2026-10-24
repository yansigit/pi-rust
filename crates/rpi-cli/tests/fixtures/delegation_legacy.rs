//! Frozen dependency-free ABI-v4 prefix, ending before all delegation slots.
use std::ffi::c_void;
#[repr(C)]
pub struct StringRef {
    data: *const u8,
    len: usize,
}
impl StringRef {
    fn new(s: &str) -> Self {
        Self {
            data: s.as_ptr(),
            len: s.len(),
        }
    }
}
#[repr(C)]
pub struct StringValue {
    data: *mut u8,
    len: usize,
}
type Request = extern "C" fn(StringRef, *mut StringValue, *mut c_void) -> i32;
type Free = extern "C" fn(StringValue);
#[repr(C)]
pub struct Prefix {
    abi_version: u32,
    struct_size: u32,
    free_string: Free,
    // Unused frozen slots retain pointer layout without importing today's types.
    register_tool: Option<extern "C" fn()>,
    register_command: Option<extern "C" fn()>,
    register_shortcut: Option<extern "C" fn()>,
    register_flag: Option<extern "C" fn()>,
    register_provider:
        Option<extern "C" fn(StringRef, StringRef, StringRef, Request, Free, *mut c_void) -> i32>,
    register_message_renderer: Option<extern "C" fn()>,
    register_markdown_transformer: Option<extern "C" fn()>,
    register_entry_renderer: Option<extern "C" fn()>,
    register_event_handler: Option<extern "C" fn()>,
    register_resources_discover: Option<extern "C" fn()>,
    runtime_action: extern "C" fn(u32, StringRef, *mut StringValue, *mut c_void) -> i32,
    dispatch_event: Option<extern "C" fn()>,
    user_data: *mut c_void,
    declare: Option<extern "C" fn(StringRef) -> i32>,
}
extern "C" fn free(value: StringValue) {
    if !value.data.is_null() {
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                value.data, value.len,
            )));
        }
    }
}
extern "C" fn request(_: StringRef, out: *mut StringValue, _: *mut c_void) -> i32 {
    let mut bytes = br#"{"role":"assistant","content":[{"type":"text","text":"legacy executed"}],"api":"faux","provider":"legacy","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":0}"#.to_vec().into_boxed_slice();
    unsafe {
        *out = StringValue {
            data: bytes.as_mut_ptr(),
            len: bytes.len(),
        };
    }
    std::mem::forget(bytes);
    0
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
    let rc = api.declare.unwrap()(StringRef::new(
        r#"{"providers":[{"id":"legacy","models":[{"id":"m","name":"Legacy","api":"faux","provider":"legacy","baseUrl":"https://unused.invalid","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":4096,"maxTokens":1024}]}]}"#,
    ));
    if rc != 0 {
        return rc;
    }
    api.register_provider.unwrap()(
        StringRef::new("legacy"),
        StringRef::new("https://unused.invalid"),
        StringRef::new("faux"),
        request,
        free,
        std::ptr::null_mut(),
    )
}
