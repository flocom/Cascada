#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod core;
mod connectors;
mod commands;

use crate::core::state::AppState;
use std::sync::Arc;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("cascada=info".parse().unwrap()))
        .init();

    let state = Arc::new(AppState::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(state.clone())
        .setup(move |app| {
            let handle = app.handle().clone();
            state.attach_app_handle(handle.clone());
            let s = state.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = s.load_from_disk().await {
                    tracing::warn!("failed to load state: {e}");
                }
                s.start_engine().await;
                s.spawn_save_loop();
                s.reconnect_all();
                s.spawn_ctrader_discovery();
                s.spawn_mt_discovery();
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_accounts,
            commands::add_account,
            commands::remove_account,
            commands::connect_account,
            commands::disconnect_account,
            commands::set_role,
            commands::rename_account,
            commands::list_rules,
            commands::upsert_rule,
            commands::delete_rule,
            commands::list_trades,
            commands::export_settings,
            commands::import_settings,
            commands::install_ctrader_bot,
            commands::install_ctrader_bot_at,
            commands::install_mt_ea_at,
            commands::install_mt_ea,
            commands::check_ea_versions,
            commands::subscribe_symbols,
            commands::list_quotes,
            commands::list_subscriptions,
            commands::request_symbols,
            commands::list_symbols,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Cascada");
}
