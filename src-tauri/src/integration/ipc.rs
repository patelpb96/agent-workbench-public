//! Frozen `invoke(name, { request })` seam; wrappers never block a UI/executor.
use super::Coordinator;
use crate::execution::Error;
use serde_json::Value;

macro_rules! wrappers {
    ($(($name:ident, $variant:ident, $ty:ty, $validator:path),)*) => { $(
        #[tauri::command]
        pub async fn $name(state: tauri::State<'_, Coordinator>, request: Value) -> Result<Value, Error> {
            // Admission is synchronous and bounded; only admitted work gets a
            // single awaitable reply. No spawn_blocking pool or unbounded tasks.
            state.submit(stringify!($name), request)?.await
        }
    )* };
}
super::request::execution_commands!(wrappers);
