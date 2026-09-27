use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(
        QmlModule::new("com.vibemix.spike0c")
            .version(1, 0)
            .qml_file("qml/main.qml"),
    )
    .files(["src/cxxqt_object.rs"])
    .build();
}
