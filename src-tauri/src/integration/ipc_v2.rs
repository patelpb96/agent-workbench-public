//! v2 `invoke(name, { request })` seam; wrappers never block a UI/executor.
use super::CoordinatorV2;
use crate::execution::Error;
use serde_json::Value;

macro_rules! wrappers {
    ($(($name:ident, $variant:ident, $ty:ty, $validator:path),)*) => { $(
        #[tauri::command]
        pub async fn $name(state: tauri::State<'_, CoordinatorV2>, request: Value) -> Result<Value, Error> {
            state.submit(stringify!($name), request)?.await
        }
    )* };
}
super::request_v2::execution_commands_v2!(wrappers);
