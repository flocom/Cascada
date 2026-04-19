use super::util::{err, S};
use crate::core::persistence::Snapshot;
use crate::core::state::AppState;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize)]
pub struct ImportReport {
    pub accounts: usize,
    pub rules: usize,
}

/// Settings file written by `export_settings`. The version field gives us
/// a forward-compatibility hook if we change the on-disk shape.
#[derive(Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    exported_at: i64,
    #[serde(default)]
    app: String,
    #[serde(flatten)]
    snapshot: Snapshot,
}

#[tauri::command]
pub async fn export_settings(state: S<'_>, path: String) -> Result<String, String> {
    let file = SettingsFile {
        version: 1,
        exported_at: chrono::Utc::now().timestamp(),
        app: "Cascada".into(),
        snapshot: state.snapshot(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(err)?;
    tokio::fs::write(&path, bytes).await.map_err(err)?;
    Ok(path)
}

#[tauri::command]
pub async fn import_settings(state: S<'_>, path: String) -> Result<ImportReport, String> {
    let bytes = tokio::fs::read(&path).await.map_err(err)?;
    let file: SettingsFile = serde_json::from_slice(&bytes)
        .map_err(|e| format!("invalid settings file: {e}"))?;
    let report = ImportReport {
        accounts: file.snapshot.accounts.len(),
        rules: file.snapshot.rules.len(),
    };
    let st: Arc<AppState> = state.inner().clone();
    st.replace_with(file.snapshot).await;
    st.reconnect_all();
    Ok(report)
}
