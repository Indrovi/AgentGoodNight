//! AgentGoodNight desktop shell: window, tray, commands and the background
//! monitor thread. All decisions are made in `agn-core`/`agn-adapters`.

mod commands;
mod engine;
mod tray;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{Manager, RunEvent, WindowEvent};

pub use engine::Engine;

/// Set when the user closes the main window, so the exit request that
/// follows the last window closing is turned into "keep running in the tray".
static CLOSING_TO_TRAY: AtomicBool = AtomicBool::new(false);

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
            commands::set_watch_wsl,
            commands::install_integration,
            commands::uninstall_integration,
        ])
        .setup(move |app| {
            tracing::info!("setting up window and tray");
            tray::build_main(app.handle())?;
            tray::create(app.handle())?;
            engine::spawn_loop(app.handle().clone(), engine.clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { .. } = event {
                if window.label() == "main" {
                    CLOSING_TO_TRAY.store(true, Ordering::Release);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build the Tauri application");

    app.run(|handle, event| match event {
        // Closing the window destroys it (freeing the webview's memory) but
        // keeps monitoring alive in the tray. Quit from the tray or the OS
        // still ends the app.
        RunEvent::ExitRequested { api, code: None, .. } if CLOSING_TO_TRAY.swap(false, Ordering::AcqRel) => {
            api.prevent_exit()
        }
        RunEvent::Exit => {
            if let Some(engine) = handle.try_state::<Arc<Engine>>() {
                engine.persist(true);
            }
        }
        _ => {}
    });
}
