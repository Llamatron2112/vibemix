import QtQuick
import QtQuick.Controls
import com.vibemix.spike0c 1.0

ApplicationWindow {
    visible: true
    width: 480
    height: 320
    title: "vibemix spike 0c"

    Backend {
        id: backend
        number: 1
        text: "hello"
    }

    Column {
        anchors.centerIn: parent
        spacing: 8

        Text {
            text: "count: " + backend.number + " (" + backend.text + ")"
        }

        Button {
            text: "increment"
            onClicked: backend.incrementNumber()
        }
    }
}
