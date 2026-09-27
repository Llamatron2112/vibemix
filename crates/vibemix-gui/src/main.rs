pub mod backend;

use std::pin::Pin;

use cxx_qt::casting::Upcast;
use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQmlEngine, QUrl};

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let port: u16 = std::env::var("VIBEMIX_OSC_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(9000);
    backend::init(port);

    let mut app = QGuiApplication::new();
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
