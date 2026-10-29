//! AgentGoodNight desktop shell: window, tray, commands and the background
//! monitor thread. All decisions are made in `agn-core`/`agn-adapters`.

mod commands;
mod engine;
mod i18n;
mod tray;

use std::sync::Arc;

use tauri::{Manager, RunEvent, WindowEvent};

pub use engine::Engine;

pub fn run() {
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).with_target(false).try_init();

    let engine = Arc::new(Engine::new());
    tracing::info!(backend = ?engine.backend(), data_dir = %engine.data_dir().display(), "starting AgentGoodNight");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main(app);
        }))
        .manage(engine.clone())
        .invoke_handler(tauri::generate_handler![
            commands::get_snapshot,
            commands::get_strings,
            commands::set_armed,
            commands::cancel_countdown,
            commands::set_action,
            commands::set_language,
            commands::set_session_ignored,
            commands::ignore_process,
            commands::set_agent_enabled,
            commands::install_integration,
            commands::uninstall_integration,
        ])
        .setup(move |app| {
            tray::create(app.handle())?;
            engine::spawn_loop(app.handle().clone(), engine.clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps monitoring alive in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build the Tauri application");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            if let Some(engine) = handle.try_state::<Arc<Engine>>() {
                engine.persist(true);
            }
        }
    });
}
