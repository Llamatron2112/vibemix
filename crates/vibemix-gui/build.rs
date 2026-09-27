use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    unsafe {
        CxxQtBuilder::new_qml_module(
            QmlModule::new("com.vibemix.gui")
                .version(1, 0)
                .qml_file("qml/main.qml"),
        )
        .files(["src/backend.rs"])
        .cc_builder(|cc| {
            cc.flag_if_supported("-Wno-sfinae-incomplete");
        })
        .build();
    }
}
