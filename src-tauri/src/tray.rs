//! System tray: quick toggle, action choice and status tooltip.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agn_adapters::monitor::Snapshot;
use agn_core::countdown::Phase;
use agn_core::power::PowerAction;
use agn_core::time::now_millis;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindowBuilder, Wry};

use crate::engine::Engine;
use agn_core::i18n::t;

const TRAY_ID: &str = "main";

pub struct TrayItems {
    show: MenuItem<Wry>,
    monitor: CheckMenuItem<Wry>,
    sleep: CheckMenuItem<Wry>,
    shutdown: CheckMenuItem<Wry>,
    cancel: MenuItem<Wry>,
    quit: MenuItem<Wry>,
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // Closing the window destroys it, which frees the webview's memory while
    // monitoring runs in the tray; build it again from the configuration.
    // Built on its own thread: window creation on the main thread from an
    // event handler can deadlock on Windows.
    static OPENING: AtomicBool = AtomicBool::new(false);
    if OPENING.swap(true, Ordering::AcqRel) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        if app.get_webview_window("main").is_none() {
            let built = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .map(|cfg| WebviewWindowBuilder::from_config(&app, cfg).and_then(|b| b.build()));
            match built {
                Some(Ok(w)) => {
                    let _ = w.set_focus();
                }
                Some(Err(e)) => tracing::warn!(error = %e, "could not open the main window"),
                None => tracing::warn!("main window missing from the configuration"),
            }
        }
        OPENING.store(false, Ordering::Release);
    });
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let lang = app.state::<Arc<Engine>>().monitor().settings().language.clone();
    let items = TrayItems {
        show: MenuItem::with_id(app, "show", t(&lang, "tray.show"), true, None::<&str>)?,
        monitor: CheckMenuItem::with_id(app, "monitor", t(&lang, "tray.monitor"), true, false, None::<&str>)?,
        sleep: CheckMenuItem::with_id(app, "sleep", t(&lang, "tray.sleep"), true, true, None::<&str>)?,
        shutdown: CheckMenuItem::with_id(app, "shutdown", t(&lang, "tray.shutdown"), true, false, None::<&str>)?,
        cancel: MenuItem::with_id(app, "cancel", t(&lang, "tray.cancel"), false, None::<&str>)?,
        quit: MenuItem::with_id(app, "quit", t(&lang, "tray.quit"), true, None::<&str>)?,
    };
    let menu = Menu::with_items(
        app,
        &[
            &items.show,
            &PredefinedMenuItem::separator(app)?,
            &items.monitor,
            &items.cancel,
            &PredefinedMenuItem::separator(app)?,
            &items.sleep,
            &items.shutdown,
            &PredefinedMenuItem::separator(app)?,
            &items.quit,
        ],
    )?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(t(&lang, "tray.tooltipOff"))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu(app, event.id.as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    app.manage(items);
    Ok(())
}

fn on_menu(app: &AppHandle, id: &str) {
    let engine = app.state::<Arc<Engine>>().inner().clone();
    match id {
        "show" => show_main(app),
        "monitor" => {
            let mut m = engine.monitor();
            if m.controller().is_armed() {
                m.disarm();
            } else {
                m.arm(now_millis());
            }
        }
        "cancel" => {
            engine.monitor().cancel_countdown(now_millis());
        }
        "sleep" | "shutdown" => {
            let action = if id == "sleep" { PowerAction::Sleep } else { PowerAction::Shutdown };
            engine.monitor().set_action(action, now_millis());
            engine.save_settings();
        }
        "quit" => {
            engine.persist(true);
            app.exit(0);
            return;
        }
        _ => return,
    }
    engine.publish(app);
}

pub fn update(app: &AppHandle, snap: &Snapshot) {
    let lang = snap.language.as_str();
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.show.set_text(t(lang, "tray.show"));
        let _ = items.monitor.set_text(t(lang, "tray.monitor"));
        let _ = items.monitor.set_checked(snap.armed);
        let _ = items.sleep.set_text(t(lang, "tray.sleep"));
        let _ = items.sleep.set_checked(snap.action == PowerAction::Sleep);
        let _ = items.shutdown.set_text(t(lang, "tray.shutdown"));
        let _ = items.shutdown.set_checked(snap.action == PowerAction::Shutdown);
        let _ = items.cancel.set_text(t(lang, "tray.cancel"));
        let _ = items.cancel.set_enabled(matches!(snap.phase, Phase::Countdown { .. }));
        let _ = items.quit.set_text(t(lang, "tray.quit"));
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let active = snap.sessions.iter().filter(|s| s.blocking && s.relevant && !s.ignored).count();
        let tip = match snap.phase {
            Phase::Off => t(lang, "tray.tooltipOff"),
            Phase::Countdown { .. } | Phase::Verifying => t(lang, "tray.tooltipCountdown"),
            Phase::Watching => t(lang, "tray.tooltipWatching").replace("{count}", &active.to_string()),
        };
        let _ = tray.set_tooltip(Some(tip));
    }
}
