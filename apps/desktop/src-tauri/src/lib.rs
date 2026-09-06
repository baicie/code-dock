//! CodeDock Desktop（Tauri 2，§14）。
//!
//! 桥接约束（§5）：Desktop 不内嵌 Runtime、不直接访问文件系统；
//! 一切经 Local IPC 与 daemon 通信。本 crate 只做协议桥：
//! Tauri commands（请求/响应）+ `session.event` notification 转发为
//! 前端事件（流式对话与事件时间线的数据源）。

pub mod bridge;

use bridge::DaemonClient;
use serde_json::json;
use std::sync::Mutex;
use tauri::{Emitter, State};

/// 应用状态：daemon 的 IPC socket 路径（可在 UI 中修改）。
#[derive(Default)]
pub struct AppState {
    socket: Mutex<String>,
}

impl AppState {
    fn socket(&self) -> String {
        self.socket.lock().expect("socket state poisoned").clone()
    }
}

#[tauri::command]
async fn set_socket(state: State<'_, AppState>, socket: String) -> Result<(), String> {
    *state.socket.lock().expect("socket state poisoned") = socket;
    Ok(())
}

#[tauri::command]
async fn get_socket(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state.socket())
}

#[tauri::command]
async fn runtime_info(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    DaemonClient::new(state.socket())
        .call("runtime.info", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn create_session(
    state: State<'_, AppState>,
    mode: String,
    task: Option<String>,
) -> Result<serde_json::Value, String> {
    DaemonClient::new(state.socket())
        .call("session.create", json!({ "mode": mode, "task": task }))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_message(
    state: State<'_, AppState>,
    session_id: String,
    text: String,
    task_type: Option<String>,
) -> Result<serde_json::Value, String> {
    let mut params = json!({ "session_id": session_id, "text": text });
    if let Some(task_type) = task_type {
        params["task_type"] = json!(task_type);
    }
    DaemonClient::new(state.socket())
        .call("session.message", params)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn restore_checkpoint(
    state: State<'_, AppState>,
    session_id: String,
    checkpoint_id: String,
) -> Result<serde_json::Value, String> {
    DaemonClient::new(state.socket())
        .call(
            "checkpoint.restore",
            json!({
                "session_id": session_id,
                "checkpoint_id": checkpoint_id,
            }),
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_events(
    state: State<'_, AppState>,
    session_id: String,
    after_sequence: Option<u64>,
) -> Result<serde_json::Value, String> {
    DaemonClient::new(state.socket())
        .call(
            "session.events",
            json!({
                "session_id": session_id,
                "after_sequence": after_sequence.unwrap_or(0),
                "durable_only": true,
                "limit": 10_000,
            }),
        )
        .await
        .map_err(|e| e.to_string())
}

/// 订阅会话事件：daemon 的 `session.event` notification 转发为
/// 前端事件（`session-replay` / `session-event` / `session-resync`）。
#[tauri::command]
async fn subscribe_session(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    after_sequence: u64,
) -> Result<(), String> {
    let socket = state.socket();
    tokio::spawn(async move {
        let client = DaemonClient::new(socket);
        let app_for_replay = app.clone();
        let app_for_resync = app.clone();
        let result = client
            .subscribe(
                &session_id,
                after_sequence,
                move |replay| {
                    let _ = app_for_replay.emit("session-replay", &replay);
                },
                move |event| {
                    let _ = app.emit("session-event", &event);
                },
                move |missed| {
                    let _ = app_for_resync.emit("session-resync", missed);
                },
            )
            .await;
        if let Err(err) = result {
            log::warn!("订阅连接结束: {err}");
        }
    });
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState {
            socket: Mutex::new("/tmp/codedock.sock".to_string()),
        })
        .invoke_handler(tauri::generate_handler![
            set_socket,
            get_socket,
            runtime_info,
            create_session,
            send_message,
            list_events,
            restore_checkpoint,
            subscribe_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
