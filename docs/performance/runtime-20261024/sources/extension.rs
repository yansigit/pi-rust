use rpi_plugin_sdk::{StableToolSchema,StbString,StepHandle,StepResult,ToolPartialCb};
use rpi_plugin_sdk::callbacks::free_string;
use rpi_plugin_sdk::handles::ToolHandles;
use std::ffi::c_void;
static DRIVES: ToolHandles<String> = ToolHandles::new();
rpi_plugin_sdk::execute_handler! {
    fn execute(_id, params) {
        let value: serde_json::Value = serde_json::from_str(&params.to_string_lossy()).unwrap();
        DRIVES.insert(value["text"].as_str().unwrap().to_owned())
    }
}
extern "C" fn poll(handle: StepHandle,_cb:Option<ToolPartialCb>,_data:*mut c_void)->StepResult {
    let entry=DRIVES.get(handle).unwrap();
    let text=entry.lock().clone();
    StepResult::done(StbString::from_string(serde_json::json!({"content":[{"type":"text","text":text}],"details":{}}).to_string()))
}
extern "C" fn cancel(h:StepHandle) { DRIVES.cancel(h); }
extern "C" fn destroy(h:StepHandle) { drop(DRIVES.remove(h)); }
rpi_plugin_sdk::export_plugin!(|api| {
    let schema=Box::new(StableToolSchema {
        name:StbString::from_string("bench_echo_INDEX".into()),
        description:StbString::from_string("Return the supplied text unchanged.".into()),
        parameters:StbString::from_string(r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into()),
    });
    let rc=(api.register_tool.unwrap())(&*schema,execute,poll,cancel,destroy,free_string);
    drop(schema);
    rc
});
