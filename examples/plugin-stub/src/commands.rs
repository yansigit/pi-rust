//! Actual loaded asynchronous command, private dialog and ordered notification fixture.
use rpi_plugin_sdk::*;
use serde_json::{json, Value};
use std::ffi::c_void;
extern "C" fn begin(_: *mut c_void) {}
extern "C" fn finish(context: *mut c_void) -> i32 {
    guard_or(1, || {
        unsafe {
            drop(Box::from_raw(context.cast::<PluginApi>()));
        }
        0
    })
}
extern "C" fn request(
    input: StbStringRef,
    control: *const AuthRequestControl,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    guard_or(1, || {
        let api = unsafe { &*context.cast::<PluginApi>() };
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let client = provider::native::Client::new(api)
            .unwrap()
            .scoped()
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let signal = rpi_ai::SimpleStreamOptions::default().signal;
        let result=runtime.block_on(async{
            let work=async {
                let owner=client.request(json!({"port":"context","action":"inspect"}),signal.clone()).await?;
                assert_eq!(owner["owner"]["child"],false);
                client.request(json!({"port":"ui","action":"notify","text":"fixture started","kind":"info"}),signal.clone()).await?;
                let answer=client.request(json!({"port":"dialogs","action":"show","ui":{"kind":"input","title":"Fixture native command","masked":true}}),signal.clone()).await?;
                if !signal.is_cancelled(){client.request(json!({"port":"ui","action":"notify","text":"fixture completed","kind":"warning"}),signal.clone()).await?;}
                Ok::<_,String>(json!({"answered":!answer.is_null(),"args":input["args"]}))
            };
            tokio::pin!(work);
            let mut tick=tokio::time::interval(std::time::Duration::from_millis(5));
            loop {tokio::select!{value=&mut work=>break value,_=tick.tick()=>{
                let control=unsafe{&*control};if (control.is_cancelled)(control.user_data)!=0{signal.cancel();}
            }}}
        });
        let value = match result {
            Ok(value) => json!({"version":1,"result":value}),
            Err(error) => json!({"version":1,"error":error}),
        };
        unsafe {
            *out = StbString::from_string(value.to_string());
        }
        0
    })
}
pub fn register(api: &PluginApi) -> Result<(), String> {
    let context = Box::into_raw(Box::new(*api)).cast();
    if api.register_plugin_lifecycle.ok_or("missing lifecycle")?(begin, finish, context) != 0 {
        unsafe {
            drop(Box::from_raw(context.cast::<PluginApi>()));
        }
        return Err("command lifecycle failed".into());
    }
    if api
        .register_native_command
        .ok_or("missing native command registration")?(
        StbStringRef::from_str(
            r#"{"version":1,"name":"native-fixture","description":"Awaited native command fixture"}"#,
        ),
        request,
        callbacks::free_string,
        context,
    ) != 0
    {
        return Err("command registration failed".into());
    }
    Ok(())
}
