use super::util::S;
use crate::core::model::Quote;

/// Replace the symbol-streaming subscription set for a single account.
/// Empty `symbols` stops the stream. Returns the canonicalised list that was
/// actually pushed to the EA (uppercased, deduped, trimmed).
#[tauri::command]
pub async fn subscribe_symbols(
    state: S<'_>,
    account_id: String,
    symbols: Vec<String>,
) -> Result<Vec<String>, String> {
    Ok(state.set_subscription(&account_id, symbols).await)
}

#[tauri::command]
pub async fn list_quotes(state: S<'_>) -> Result<Vec<Quote>, String> {
    Ok(state.quotes.iter().map(|kv| kv.value().clone()).collect())
}

#[tauri::command]
pub async fn list_subscriptions(state: S<'_>) -> Result<Vec<(String, Vec<String>)>, String> {
    Ok(state.subscriptions.iter()
        .map(|kv| (kv.key().clone(), kv.value().clone()))
        .collect())
}

/// Ask the EA for its current symbol watchlist. Reply arrives async on the
/// `cascada://symbols` event; the cached result is fetchable via `list_symbols`.
#[tauri::command]
pub async fn request_symbols(state: S<'_>, account_id: String) -> Result<bool, String> {
    Ok(state.request_symbols(&account_id).await)
}

#[tauri::command]
pub async fn list_symbols(state: S<'_>, account_id: String) -> Result<Vec<String>, String> {
    Ok(state.symbols.get(&account_id).map(|v| v.clone()).unwrap_or_default())
}
