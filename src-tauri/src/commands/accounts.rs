use super::util::{err, S};
use crate::core::model::*;
use serde::Deserialize;
use uuid::Uuid;

#[tauri::command]
pub async fn list_accounts(state: S<'_>) -> Result<Vec<Account>, String> {
    let mut out: Vec<Account> = state.accounts.iter().map(|kv| kv.value().clone()).collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
}

#[derive(Deserialize)]
pub struct AddAccountPayload {
    pub platform: Platform,
    pub label: String,
    pub login: String,
    pub server: String,
    pub role: AccountRole,
    pub password: Option<String>,
}

#[tauri::command]
pub async fn add_account(state: S<'_>, payload: AddAccountPayload) -> Result<Account, String> {
    let account = Account {
        id: Uuid::new_v4().to_string(),
        platform: payload.platform,
        label: payload.label,
        login: payload.login,
        server: payload.server,
        role: payload.role,
        connected: false,
        balance: 0.0,
        equity: 0.0,
        currency: "USD".into(),
        password: payload.password,
    };
    state.accounts.insert(account.id.clone(), account.clone());
    state.mark_dirty();
    Ok(account)
}

#[tauri::command]
pub async fn remove_account(state: S<'_>, id: String) -> Result<(), String> {
    state.disconnect(&id).await.ok();
    state.accounts.remove(&id);
    state.mark_dirty();
    Ok(())
}

#[tauri::command]
pub async fn connect_account(state: S<'_>, id: String) -> Result<(), String> {
    state.connect(&id).await.map_err(err)
}

#[tauri::command]
pub async fn disconnect_account(state: S<'_>, id: String) -> Result<(), String> {
    state.disconnect(&id).await.map_err(err)
}

#[tauri::command]
pub async fn rename_account(state: S<'_>, id: String, label: String) -> Result<(), String> {
    let mut updated: Option<Account> = None;
    if let Some(mut a) = state.accounts.get_mut(&id) {
        if a.label != label { a.label = label; updated = Some(a.clone()); }
    }
    if let Some(a) = updated { state.mark_dirty(); state.emit_account_public(&a); }
    Ok(())
}

#[tauri::command]
pub async fn set_role(state: S<'_>, id: String, role: AccountRole) -> Result<(), String> {
    let mut updated: Option<Account> = None;
    if let Some(mut a) = state.accounts.get_mut(&id) {
        if a.role != role { a.role = role; updated = Some(a.clone()); }
    }
    if let Some(a) = updated { state.mark_dirty(); state.emit_account_public(&a); }
    Ok(())
}
