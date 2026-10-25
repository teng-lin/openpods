// SPDX-License-Identifier: MIT
import QtQuick
import qs.Commons

Item {
  id: root
  property real iconSize: 16
  property color color: Color.foreground
  property string variant: "buds"
  implicitWidth: iconSize
  implicitHeight: iconSize

  Item {
    width: 32; height: 32
    anchors.centerIn: parent
    scale: root.iconSize / 32
    visible: root.variant !== "max"
    Repeater {
      model: 2
      delegate: Item {
        required property int index
        x: index * 17; y: 4
        Rectangle { width: 14; height: 12; radius: 6; color: root.color }
        Rectangle { x: index === 0 ? 9 : 0; y: 7; width: 5; height: 21; radius: 2.5; color: root.color }
      }
    }
  }
  Item {
    width: 32; height: 32
    anchors.centerIn: parent
    scale: root.iconSize / 32
    visible: root.variant === "max"
    Rectangle { x: 5; y: 3; width: 22; height: 25; radius: 11; color: "transparent"; border.color: root.color; border.width: 3 }
    Rectangle { x: 1; y: 15; width: 9; height: 15; radius: 4; color: root.color }
    Rectangle { x: 22; y: 15; width: 9; height: 15; radius: 4; color: root.color }
  }
}
