import QtQuick
import QtQuick.Layouts
import QtQuick.Controls
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "arch.gemini-router"

  // Reactive Status Properties
  property bool routerRunning: false
  property bool serviceEnabled: false
  property string routerUrl: "http://127.0.0.1:4000"
  property int routerPort: 4000
  property bool claudeEnabled: false
  property bool codexEnabled: false
  property bool anyEnabled: false
  property bool bothEnabled: false
  property bool claudeInstalled: true
  property bool codexInstalled: true
  property string primaryModel: "gemini-3.8-flash"
  property string primaryModelDisplay: "Gemini 3.8 Flash"
  property var modelPresets: [
    { id: "gemini-3.8-flash", label: "Gemini 3.8 Flash", short: "Gemini 3.8", badge: "G3.8" },
    { id: "gemini-4-argon", label: "Gemini 4.0 Argon", short: "Gemini 4.0", badge: "G4" }
  ]
  property string reasoningEffort: "medium"
  property bool hotSwapActive: false
  property string statusLabel: "Direct (Routing Off)"
  property string badgeText: "ADC Off"

  // ADC status
  property bool adcValid: false
  property bool adcAuthAlert: false
  property int adcStatusCode: 0
  property string adcProject: "unknown"
  property string adcLocation: "global"
  property bool apiKeyUsed: false
  property string adcError: ""
  property bool authPopupShown: false

  // Live session telemetry
  property int totalRequests: 0
  property int claudeRequests: 0
  property int codexRequests: 0
  property int totalTokens: 0
  property int reasoningTokens: 0
  property string lastClient: ""
  property string lastVertexModel: ""
  property int lastLatencyMs: 0

  // UI state
  property bool popupOpen: false
  property bool isBusy: false
  property string notificationMessage: ""
  property bool showLogsPanel: false
  property string routerLogs: "Loading logs…"

  readonly property string ctlPath: {
    var resolved = Qt.resolvedUrl("../bin/omarchy-gemini-router-ctl")
    var localPath = decodeURIComponent(String(resolved).replace(/^file:\/\//, ""))
    return localPath
  }

  // Omarchy Shell panel contract: opened, open(), close(), toggle()
  readonly property bool opened: popupOpen
  function open() { root.openPopup() }
  function close() { root.closePopup() }
  function toggle() { root.togglePopup() }

  function openPopup() {
    root.popupOpen = true
    if (root.bar && typeof root.bar.requestPopout === "function") {
      root.bar.requestPopout(root)
    }
    root.refreshStatus()
  }

  function closePopup() {
    root.popupOpen = false
    root.showLogsPanel = false
  }

  function togglePopup() {
    if (root.popupOpen) {
      root.closePopup()
    } else {
      root.openPopup()
    }
  }

  onPopupOpenChanged: {
    if (popup && popup.open !== root.popupOpen) {
      popup.open = root.popupOpen
    }
  }

  function formatCompactNumber(n) {
    var v = Number(n) || 0
    if (v >= 1000000) return (v / 1000000).toFixed(1) + "M"
    if (v >= 1000) return (v / 1000).toFixed(1) + "k"
    return String(v)
  }

  function applyStatusJson(rawText) {
    try {
      var data = JSON.parse(rawText.trim())
      if (!data) return
      if (data.error) {
        root.notificationMessage = data.error
      }
      if (data.router_running !== undefined) root.routerRunning = data.router_running === true
      if (data.service_enabled !== undefined) root.serviceEnabled = data.service_enabled === true
      if (data.router_url) root.routerUrl = data.router_url
      if (data.router_port) root.routerPort = data.router_port
      if (data.claude_enabled !== undefined) root.claudeEnabled = data.claude_enabled === true
      if (data.codex_enabled !== undefined) root.codexEnabled = data.codex_enabled === true
      if (data.any_enabled !== undefined) root.anyEnabled = data.any_enabled === true
      if (data.both_enabled !== undefined) root.bothEnabled = data.both_enabled === true
      if (data.claude_installed !== undefined) root.claudeInstalled = data.claude_installed === true
      if (data.codex_installed !== undefined) root.codexInstalled = data.codex_installed === true
      if (data.primary_model) root.primaryModel = data.primary_model
      if (data.primary_model_display) root.primaryModelDisplay = data.primary_model_display
      if (Array.isArray(data.model_presets) && data.model_presets.length > 0) {
        root.modelPresets = data.model_presets
      }
      if (data.reasoning_effort) root.reasoningEffort = data.reasoning_effort
      if (data.hot_swap_active !== undefined) root.hotSwapActive = data.hot_swap_active === true
      if (data.status_label) root.statusLabel = data.status_label
      if (data.badge_text) root.badgeText = data.badge_text
      if (data.adc) {
        root.adcValid = data.adc.token_valid === true
        root.adcAuthAlert = data.adc.auth_alert === true
        root.adcStatusCode = data.adc.auth_status_code || 0
        root.adcProject = data.adc.project_id || "unknown"
        root.adcLocation = data.adc.location || "global"
        root.apiKeyUsed = data.adc.api_key_used === true
        root.adcError = data.adc.error || ""
        if (root.adcValid && !root.adcAuthAlert) {
          root.authPopupShown = false
        } else if (root.anyEnabled && (!root.adcValid || root.adcAuthAlert) && !root.authPopupShown) {
          root.authPopupShown = true
          root.openPopup()
        }
      }
      if (data.stats) {
        root.totalRequests = data.stats.total_requests || 0
        root.claudeRequests = data.stats.claude_requests || 0
        root.codexRequests = data.stats.codex_requests || 0
        root.totalTokens = data.stats.total_tokens || 0
        root.reasoningTokens = data.stats.reasoning_tokens || 0
        root.lastClient = data.stats.last_client || ""
        root.lastVertexModel = data.stats.last_vertex_model || ""
        root.lastLatencyMs = data.stats.last_latency_ms || 0
      }
    } catch(e) {
      // ignore parse errors
    }
  }

  function refreshStatus() {
    if (!statusProc.running) {
      statusProc.running = true
    }
  }

  function runAction(argsList, notifyText) {
    if (actionProc.running) return
    root.isBusy = true
    if (notifyText) root.notificationMessage = notifyText
    actionProc.command = [root.ctlPath].concat(argsList)
    actionProc.running = true
  }

  function fetchLogs() {
    if (!logsProc.running) {
      logsProc.running = true
    }
  }

  readonly property string displayIcon: root.anyEnabled ? "󰚩" : "󰘦"

  readonly property string displayText: {
    if (root.vertical) return root.displayIcon
    if (root.isBusy) return root.displayIcon + " …"
    if (root.anyEnabled && (!root.adcValid || root.adcAuthAlert)) return "⚠ Auth Reset"
    return root.displayIcon + " " + root.badgeText
  }

  readonly property color widgetColor: {
    if (root.isBusy) return "#f9e2af"
    if (root.anyEnabled && (!root.routerRunning || !root.adcValid || root.adcAuthAlert)) return "#f38ba8"
    if (root.bothEnabled) return "#a6e3a1"
    if (root.anyEnabled) return Color.accent
    return Color.foreground
  }

  readonly property string tooltipDetails: {
    if (root.popupOpen) return ""
    var t = "󰚩 Gemini ADC Router\n"
    t += "───────────────────────────\n"
    t += "Mode: " + root.statusLabel + "\n"
    t += "Model: " + root.primaryModel + " (" + root.reasoningEffort + ")\n"
    t += "Claude Code: " + (root.claudeEnabled ? "Routed (/v1/messages)" : "Direct") + "\n"
    t += "OpenAI Codex: " + (root.codexEnabled ? "Routed (/v1/responses)" : "Direct") + "\n"
    t += "Router Daemon: " + (root.routerRunning ? "Online (" + root.routerUrl + ")" : "Stopped") + "\n"
    t += "Vertex AI ADC: " + (root.adcValid ? "Valid (" + root.adcProject + ")" : "Auth required (401/403)") + "\n"
    t += "Session Stats: " + root.totalRequests + " reqs · " + root.formatCompactNumber(root.totalTokens) + " tokens\n"
    t += "───────────────────────────\n"
    t += "Left-click: Open Control Panel\n"
    t += "Right-click: Quick Toggle Both\n"
    t += "Middle-click: Cycle active Gemini model"
    return t
  }

  Timer {
    id: pollTimer
    interval: root.popupOpen ? 4000 : 8000
    repeat: true
    running: true
    triggeredOnStart: true
    onTriggered: root.refreshStatus()
  }

  Timer {
    id: notifyTimer
    interval: 4000
    repeat: false
    running: root.notificationMessage !== ""
    onTriggered: root.notificationMessage = ""
  }

  Process {
    id: statusProc
    command: [root.ctlPath, "status"]
    running: false
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        root.applyStatusJson(text)
      }
    }
  }

  Process {
    id: actionProc
    command: [root.ctlPath, "status"]
    running: false
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        root.isBusy = false
        root.applyStatusJson(text)
      }
    }
  }

  Process {
    id: logsProc
    command: [root.ctlPath, "logs", "--lines", "40"]
    running: false
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        try {
          var res = JSON.parse(text.trim())
          root.routerLogs = res.text || "No logs available."
        } catch(e) {
          root.routerLogs = "Error loading logs."
        }
      }
    }
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.displayText
    fontSize: Style.font.caption
    horizontalMargin: 6
    tooltipText: root.tooltipDetails
    active: root.anyEnabled
    activeColor: root.widgetColor
    onPressed: function(btn) {
      if (btn === Qt.RightButton) {
        root.runAction(
          ["toggle", "--target", "both"],
          root.bothEnabled ? "Disabling Claude & Codex routing…" : "Routing Claude & Codex to Gemini…"
        )
      } else if (btn === Qt.MiddleButton) {
        root.runAction(
          ["set-model", "--flip"],
          "Flipped active Gemini model"
        )
      } else {
        root.togglePopup()
      }
    }
  }

  PopupCard {
    id: popup
    anchorItem: root
    bar: root.bar
    owner: root
    open: root.popupOpen
    onOpenChanged: {
      if (open !== root.popupOpen) {
        root.popupOpen = open
      }
    }
    contentWidth: popup.fittedContentWidth(Style.space(440))
    contentHeight: popup.fittedContentHeight(panelContent.implicitHeight)

    FocusScope {
      id: focusScope
      anchors.fill: parent
      implicitHeight: panelContent.implicitHeight
      implicitWidth: panelContent.implicitWidth
      focus: root.popupOpen
      Keys.onEscapePressed: function(event) {
        root.closePopup()
        event.accepted = true
      }

      ColumnLayout {
        id: panelContent
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: Style.space(10)

        // ──────────────── Header ────────────────
        RowLayout {
          Layout.fillWidth: true
          spacing: Style.space(8)

          Text {
            text: "󰚩"
            font.family: Style.font.family
            font.pixelSize: Style.font.title
            color: Color.accent
            Layout.alignment: Qt.AlignVCenter
          }

          Text {
            text: "Gemini ADC Router"
            font.family: Style.font.family
            font.pixelSize: Style.font.title
            font.bold: true
            color: Color.foreground
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignVCenter
          }

          // Status Pill
          Rectangle {
            implicitHeight: Style.space(22)
            implicitWidth: statusBadgeText.implicitWidth + Style.space(16)
            radius: Style.space(11)
            Layout.alignment: Qt.AlignVCenter
            color: root.anyEnabled
              ? Qt.rgba(0.65, 0.89, 0.63, 0.2)
              : Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.08)
            border.width: 1
            border.color: root.anyEnabled ? "#a6e3a1" : Color.muted

            Text {
              id: statusBadgeText
              anchors.centerIn: parent
              textFormat: Text.PlainText
              text: root.anyEnabled ? ("● " + root.badgeText) : "○ Direct Mode"
              font.family: Style.font.family
              font.pixelSize: Style.font.caption
              color: root.anyEnabled ? "#a6e3a1" : Color.muted
            }
          }

          Button {
            iconText: ""
            tooltipText: "Refresh status"
            fontSize: Style.font.caption
            horizontalPadding: Style.space(6)
            verticalPadding: Style.space(4)
            Layout.alignment: Qt.AlignVCenter
            onClicked: root.refreshStatus()
          }
        }

        // Notification Banner
        Rectangle {
          Layout.fillWidth: true
          implicitHeight: notifyMsg.implicitHeight + Style.space(10)
          visible: root.notificationMessage !== ""
          radius: Style.space(6)
          color: Qt.rgba(Color.accent.r, Color.accent.g, Color.accent.b, 0.15)
          border.width: 1
          border.color: Color.accent

          Text {
            id: notifyMsg
            anchors.centerIn: parent
            textFormat: Text.PlainText
            text: root.notificationMessage
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            color: Color.accent
          }
        }

        // 401 / 403 Credential Reset Alert Popup Banner
        BorderSurface {
          Layout.fillWidth: true
          visible: !root.adcValid || root.adcAuthAlert
          implicitHeight: authAlertCol.implicitHeight + topPadding + bottomPadding
          radius: Style.space(6)
          color: Qt.rgba(0.95, 0.55, 0.66, 0.14)
          border.width: 1
          border.color: "#f38ba8"
          topPadding: Style.space(10)
          bottomPadding: Style.space(10)
          leftPadding: Style.space(12)
          rightPadding: Style.space(12)

          ColumnLayout {
            id: authAlertCol
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.leftMargin: parent.leftPadding
            anchors.rightMargin: parent.rightPadding
            anchors.topMargin: parent.topPadding
            spacing: Style.space(6)

            Text {
              Layout.fillWidth: true
              textFormat: Text.PlainText
              text: "⚠ Vertex AI Auth Issue (" + (root.adcStatusCode > 0 ? ("HTTP " + root.adcStatusCode) : "401 / 403") + ") — Reset Credential"
              font.family: Style.font.family
              font.pixelSize: Style.font.caption
              font.bold: true
              color: "#f38ba8"
              wrapMode: Text.WordWrap
            }

            Text {
              Layout.fillWidth: true
              textFormat: Text.PlainText
              text: root.adcError !== ""
                ? root.adcError
                : "Auto-heal could not refresh Vertex AI ADC credentials. Please reset your ADC login below."
              font.family: Style.font.family
              font.pixelSize: Style.font.caption
              color: Color.foreground
              wrapMode: Text.WordWrap
            }

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(8)

              Button {
                Layout.fillWidth: true
                Layout.preferredHeight: Style.space(28)
                text: "Reset Credential (ADC Login)"
                bordered: true
                accent: "#f38ba8"
                selected: true
                enabled: !root.isBusy
                onClicked: root.runAction(
                  ["adc-login"],
                  "Launching gcloud ADC credential reset…"
                )
              }

              Button {
                Layout.preferredHeight: Style.space(28)
                text: "Retry Auto-Heal"
                bordered: true
                enabled: !root.isBusy
                onClicked: root.runAction(
                  ["refresh-auth"],
                  "Clearing token cache & re-checking ADC…"
                )
              }
            }
          }
        }

        PanelSeparator { Layout.fillWidth: true }

        // ──────────────── Client Routing Toggles ────────────────
        BorderSurface {
          Layout.fillWidth: true
          implicitHeight: routingCol.implicitHeight + topPadding + bottomPadding
          radius: Style.space(6)
          color: Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.04)
          border.width: 1
          border.color: Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.1)
          topPadding: Style.space(10)
          bottomPadding: Style.space(10)
          leftPadding: Style.space(12)
          rightPadding: Style.space(12)

          ColumnLayout {
            id: routingCol
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.leftMargin: parent.leftPadding
            anchors.rightMargin: parent.rightPadding
            anchors.topMargin: parent.topPadding
            spacing: Style.space(8)

            // Master Quick Action Row
            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(8)

              Button {
                Layout.fillWidth: true
                Layout.preferredHeight: Style.space(30)
                text: "Route Both to Gemini"
                iconText: "󰚩"
                bordered: true
                accent: "#a6e3a1"
                selected: root.bothEnabled
                enabled: !root.isBusy
                onClicked: root.runAction(["enable", "--target", "both"], "Routing Claude & Codex to Gemini…")
              }

              Button {
                Layout.fillWidth: true
                Layout.preferredHeight: Style.space(30)
                text: "Disable All (Direct)"
                iconText: "󰅖"
                bordered: true
                selected: !root.anyEnabled
                enabled: !root.isBusy && root.anyEnabled
                onClicked: root.runAction(["disable", "--target", "both"], "Restoring direct Claude & Codex config…")
              }
            }

            // Claude Code Toggle Row
            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(8)

              ColumnLayout {
                Layout.fillWidth: true
                spacing: Style.space(2)
                Text {
                  text: "Claude Code (claude)"
                  font.bold: true
                  color: Color.foreground
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                }
                Text {
                  text: root.claudeEnabled
                    ? ("Routed -> " + root.primaryModel + " (/v1/messages)")
                    : "Direct (~/.claude/settings.json)"
                  color: root.claudeEnabled ? "#a6e3a1" : Color.muted
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                }
              }

              Button {
                Layout.preferredHeight: Style.space(28)
                text: root.claudeEnabled ? "ON (Routed)" : "OFF (Direct)"
                bordered: true
                accent: root.claudeEnabled ? "#a6e3a1" : Color.accent
                selected: root.claudeEnabled
                enabled: !root.isBusy
                onClicked: root.runAction(
                  ["toggle", "--target", "claude"],
                  root.claudeEnabled ? "Disabling Claude routing…" : "Routing Claude Code to Gemini…"
                )
              }
            }

            // OpenAI Codex Toggle Row
            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(8)

              ColumnLayout {
                Layout.fillWidth: true
                spacing: Style.space(2)
                Text {
                  text: "OpenAI Codex (codex)"
                  font.bold: true
                  color: Color.foreground
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                }
                Text {
                  text: root.codexEnabled
                    ? ("Routed -> " + root.primaryModel + " (/v1/responses)")
                    : "Direct (~/.codex/config.toml)"
                  color: root.codexEnabled ? "#a6e3a1" : Color.muted
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                }
              }

              Button {
                Layout.preferredHeight: Style.space(28)
                text: root.codexEnabled ? "ON (Routed)" : "OFF (Direct)"
                bordered: true
                accent: root.codexEnabled ? "#a6e3a1" : Color.accent
                selected: root.codexEnabled
                enabled: !root.isBusy
                onClicked: root.runAction(
                  ["toggle", "--target", "codex"],
                  root.codexEnabled ? "Disabling Codex routing…" : "Routing OpenAI Codex to Gemini…"
                )
              }
            }
          }
        }

        // ──────────────── Model & Effort Selector Card ────────────────
        BorderSurface {
          Layout.fillWidth: true
          implicitHeight: modelCol.implicitHeight + topPadding + bottomPadding
          radius: Style.space(6)
          color: Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.04)
          border.width: 1
          border.color: Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.1)
          topPadding: Style.space(10)
          bottomPadding: Style.space(10)
          leftPadding: Style.space(12)
          rightPadding: Style.space(12)

          ColumnLayout {
            id: modelCol
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.leftMargin: parent.leftPadding
            anchors.rightMargin: parent.rightPadding
            anchors.topMargin: parent.topPadding
            spacing: Style.space(6)

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(6)
              Text {
                text: "Active Vertex AI Gemini Model (" + root.primaryModel + ")"
                font.bold: true
                color: Color.muted
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                Layout.fillWidth: true
                elide: Text.ElideRight
              }
              Text {
                text: "Hot-Swap Ready"
                color: root.hotSwapActive ? "#a6e3a1" : Color.muted
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
              }
            }

            GridLayout {
              Layout.fillWidth: true
              columns: 2
              columnSpacing: Style.space(8)
              rowSpacing: Style.space(6)

              Repeater {
                model: root.modelPresets
                delegate: Button {
                  required property var modelData
                  Layout.fillWidth: true
                  Layout.preferredHeight: Style.space(28)
                  text: modelData.label || modelData.id
                  tooltipText: modelData.id || ""
                  bordered: true
                  accent: Color.accent
                  selected: root.primaryModel === modelData.id
                  enabled: !root.isBusy
                  onClicked: root.runAction(
                    ["set-model", "--model", modelData.id],
                    "Switched model to " + modelData.id
                  )
                }
              }
            }

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(6)

              Rectangle {
                Layout.fillWidth: true
                Layout.preferredHeight: Style.space(26)
                radius: Style.space(4)
                color: Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.06)
                border.width: 1
                border.color: customModelInput.activeFocus
                  ? Color.accent
                  : Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.16)

                TextInput {
                  id: customModelInput
                  anchors.fill: parent
                  anchors.leftMargin: Style.space(8)
                  anchors.rightMargin: Style.space(8)
                  verticalAlignment: TextInput.AlignVCenter
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                  color: Color.foreground
                  clip: true
                  selectByMouse: true
                  onAccepted: {
                    var val = customModelInput.text.trim()
                    if (val.length > 0 && !root.isBusy) {
                      root.runAction(
                        ["set-model", "--model", val],
                        "Switched model to " + val
                      )
                      customModelInput.text = ""
                    }
                  }

                  Text {
                    anchors.fill: parent
                    verticalAlignment: Text.AlignVCenter
                    text: "Custom model ID (e.g. gemini-2.5-pro, gemini-3.1-pro-preview)…"
                    font.family: Style.font.family
                    font.pixelSize: Style.font.caption
                    color: Color.muted
                    visible: customModelInput.text.length === 0 && !customModelInput.activeFocus
                    elide: Text.ElideRight
                  }
                }
              }

              Button {
                Layout.preferredHeight: Style.space(26)
                text: "Use Model"
                bordered: true
                accent: Color.accent
                enabled: !root.isBusy && customModelInput.text.trim().length > 0
                onClicked: {
                  var val = customModelInput.text.trim()
                  if (val.length > 0) {
                    root.runAction(
                      ["set-model", "--model", val],
                      "Switched model to " + val
                    )
                    customModelInput.text = ""
                  }
                }
              }
            }

            Text {
              text: "Reasoning Effort (Thinking Level)"
              font.bold: true
              color: Color.muted
              font.family: Style.font.family
              font.pixelSize: Style.font.caption
            }

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(6)

              Repeater {
                model: ["low", "medium", "high", "xhigh"]
                delegate: Button {
                  required property string modelData
                  Layout.fillWidth: true
                  Layout.preferredHeight: Style.space(26)
                  text: modelData.toUpperCase()
                  bordered: true
                  accent: Color.accent
                  selected: root.reasoningEffort === modelData
                  enabled: !root.isBusy
                  onClicked: root.runAction(
                    ["set-model", "--effort", modelData],
                    "Set reasoning effort to " + modelData
                  )
                }
              }
            }
          }
        }

        // ──────────────── ADC & Daemon Overview Card ────────────────
        BorderSurface {
          Layout.fillWidth: true
          implicitHeight: adcCol.implicitHeight + topPadding + bottomPadding
          radius: Style.space(6)
          color: Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.04)
          border.width: 1
          border.color: Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.1)
          topPadding: Style.space(10)
          bottomPadding: Style.space(10)
          leftPadding: Style.space(12)
          rightPadding: Style.space(12)

          ColumnLayout {
            id: adcCol
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.leftMargin: parent.leftPadding
            anchors.rightMargin: parent.rightPadding
            anchors.topMargin: parent.topPadding
            spacing: Style.space(6)

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(8)
              Text {
                text: "Router Daemon:"
                font.bold: true
                color: Color.muted
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                Layout.preferredWidth: Style.space(105)
              }
              Text {
                text: (root.routerRunning ? "Online" : "Stopped") + " (" + root.routerUrl + ")"
                color: root.routerRunning ? "#a6e3a1" : "#f38ba8"
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                Layout.fillWidth: true
              }
            }

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(8)
              Text {
                text: "Vertex AI ADC:"
                font.bold: true
                color: Color.muted
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                Layout.preferredWidth: Style.space(105)
              }
              Text {
                text: (root.adcValid ? "Token Valid" : "Needs Login") + " · " + root.adcProject + " (" + root.adcLocation + ")"
                color: root.adcValid ? "#a6e3a1" : "#f38ba8"
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                Layout.fillWidth: true
                elide: Text.ElideRight
              }
            }

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(8)
              Text {
                text: "Session Stats:"
                font.bold: true
                color: Color.muted
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                Layout.preferredWidth: Style.space(105)
              }
              Text {
                text: root.totalRequests > 0
                  ? (root.totalRequests + " reqs (C:" + root.claudeRequests + " X:" + root.codexRequests + ") · "
                    + root.formatCompactNumber(root.totalTokens) + " tok"
                    + (root.lastLatencyMs > 0 ? (" · " + root.lastLatencyMs + "ms") : ""))
                  : "0 requests routed this session"
                color: root.totalRequests > 0 ? Color.foreground : Color.muted
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                Layout.fillWidth: true
                elide: Text.ElideRight
              }
            }

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(6)

              Button {
                Layout.fillWidth: true
                Layout.preferredHeight: Style.space(26)
                text: root.routerRunning ? "Restart" : "Start Router"
                bordered: true
                enabled: !root.isBusy
                onClicked: root.runAction(
                  [root.routerRunning ? "restart-router" : "start-router"],
                  "Starting ADC router daemon…"
                )
              }

              Button {
                Layout.preferredHeight: Style.space(26)
                text: "Stop"
                bordered: true
                enabled: !root.isBusy && root.routerRunning
                onClicked: root.runAction(
                  ["stop-router"],
                  "Stopping ADC router daemon…"
                )
              }

              Button {
                Layout.fillWidth: true
                Layout.preferredHeight: Style.space(26)
                text: "ADC Re-login"
                bordered: true
                enabled: !root.isBusy
                onClicked: root.runAction(["adc-login"], "Launching gcloud ADC login…")
              }

              Button {
                Layout.preferredHeight: Style.space(26)
                text: root.showLogsPanel ? "Hide Logs" : "Logs"
                bordered: true
                onClicked: {
                  root.showLogsPanel = !root.showLogsPanel
                  if (root.showLogsPanel) root.fetchLogs()
                }
              }
            }

            Rectangle {
              visible: root.showLogsPanel
              Layout.fillWidth: true
              implicitHeight: Style.space(130)
              radius: Style.space(4)
              color: Qt.rgba(0, 0, 0, 0.35)
              border.width: 1
              border.color: Qt.rgba(Color.foreground.r, Color.foreground.g, Color.foreground.b, 0.12)
              clip: true

              Flickable {
                anchors.fill: parent
                anchors.margins: Style.space(6)
                contentWidth: width
                contentHeight: logText.implicitHeight
                clip: true

                Text {
                  id: logText
                  width: parent.width
                  textFormat: Text.PlainText
                  text: root.routerLogs
                  wrapMode: Text.WrapAnywhere
                  font.family: "monospace"
                  font.pixelSize: Style.font.caption
                  color: Color.muted
                }
              }
            }
          }
        }
      }
    }
  }
}
