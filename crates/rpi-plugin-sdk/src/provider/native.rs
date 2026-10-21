//! Private versioned host ports. Missing capabilities are explicit errors.
use crate::{callbacks::OwnedString, AuthRequestControl, PluginApi, StbString, StbStringRef};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Client {
    api: PluginApi,
}
impl Client {
    pub fn new(api: &PluginApi) -> Result<Self, String> {
        if api.native_request.is_none() {
            return Err("native runtime ports require a newer rpi host".into());
        }
        Ok(Self { api: *api })
    }
    pub async fn request(
        &self,
        mut request: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let object = request
            .as_object_mut()
            .ok_or("native request must be an object")?;
        object.insert("version".into(), 1.into());
        let api = self.api;
        let signal = signal.child_token();
        let _cancel = signal.clone().drop_guard();
        tokio::task::spawn_blocking(move || {
            let api = &api;
            let control = AuthRequestControl {
                struct_size: std::mem::size_of::<AuthRequestControl>() as u32,
                is_cancelled,
                user_data: (&signal as *const CancellationToken).cast_mut().cast(),
            };
            let mut output = StbString::empty();
            let status = (api.native_request.unwrap())(
                StbStringRef::from_str(&request.to_string()),
                &control,
                &mut output,
                api.user_data,
            );
            // Host ports invert the usual provider ownership of returned strings.
            let output = unsafe { OwnedString::from_raw(output, Some(api.free_string)) };
            let value: Value = serde_json::from_str(&output.to_string_lossy())
                .map_err(|_| "invalid native host response")?;
            if value["version"] != 1 {
                return Err("unsupported native host response".into());
            }
            if status != 0 {
                return Err(value["error"]
                    .as_str()
                    .unwrap_or("native host request failed")
                    .into());
            }
            value
                .get("result")
                .cloned()
                .ok_or_else(|| "native host response omitted its result".into())
        })
        .await
        .map_err(|_| "native host request task stopped".to_owned())?
    }
}
extern "C" fn is_cancelled(data: *mut std::ffi::c_void) -> u8 {
    u8::from(unsafe { &*data.cast::<CancellationToken>() }.is_cancelled())
}
