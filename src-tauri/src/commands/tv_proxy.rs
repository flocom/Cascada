//! Tauri commands for the in-app TradingView sidecar manager.
//!
//! These wrap [`crate::sidecar::TvProxyManager`] so the Svelte UI can drive
//! the venv install / mitmdump start-stop directly. Each command is a thin
//! async pass-through; all the heavy lifting (Python detection, pip,
//! supervised spawn) lives in the sidecar module.

use super::util::S;
use crate::sidecar::TvProxyStatus;

#[tauri::command]
pub async fn tv_proxy_status(state: S<'_>) -> Result<TvProxyStatus, String> {
    Ok(state.tv_proxy.status().await)
}

#[tauri::command]
pub async fn tv_proxy_setup(state: S<'_>) -> Result<TvProxyStatus, String> {
    state.tv_proxy.setup().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn tv_proxy_start(state: S<'_>) -> Result<TvProxyStatus, String> {
    state.tv_proxy.start().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn tv_proxy_stop(state: S<'_>) -> Result<TvProxyStatus, String> {
    state.tv_proxy.stop().await.map_err(|e| e.to_string())?;
    Ok(state.tv_proxy.status().await)
}
