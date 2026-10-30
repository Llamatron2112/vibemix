pub mod backend;

use std::pin::Pin;

use cxx_qt::casting::Upcast;
use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQmlEngine, QString, QUrl};

fn main() {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("vibemix-gui {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                println!(
                    "vibemix-gui {}\n\nUsage: vibemix-gui\n\nEnv: VIBEMIX_SOCKET (the engine's control socket; default engine.sock in $XDG_RUNTIME_DIR/vibemix)",
                    env!("CARGO_PKG_VERSION")
                );
                return;
            }
            _ => {}
        }
    }

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if let Err(error) = backend::init(vibemix_proto::socket::default_engine_socket()) {
        eprintln!("vibemix-gui: {error}");
        std::process::exit(1);
    }

    let mut app = QGuiApplication::new();

    // Wayland/XDG and KDE X11 identify a window by the base name of its
    // desktop entry; without this Qt falls back to the executable name
    // ("vibemix-gui"), which has no .desktop file, so compositors show a
    // generic window icon. "vibemix" matches packaging/vibemix.desktop,
    // whose Icon= resolves to the installed hicolor SVG.
    QGuiApplication::set_desktop_file_name(&QString::from("vibemix"));

    // Application metadata, surfaced to QML as `Qt.application.*` (the About
    // dialog shows the version).
    if let Some(mut app) = app.as_mut() {
        app.as_mut().set_application_name(&QString::from("vibemix"));
        app.as_mut()
            .set_application_display_name(&QString::from("vibemix"));
        app.as_mut()
            .set_application_version(&QString::from(env!("CARGO_PKG_VERSION")));
    }

    let mut engine = QQmlApplicationEngine::new();

    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/com/vibemix/gui/qml/main.qml"));
    }

    if let Some(engine) = engine.as_mut() {
        let engine: Pin<&mut QQmlEngine> = engine.upcast_pin();
        engine.on_quit(|_| tracing::info!("qml quit")).release();
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }
}
