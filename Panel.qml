import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import qs.Ui as Ui
import qs.Commons

Ui.Panel {
  id: root
  property var shell: null
  property var manifest: null
  property var service: null
  property string highlightedTransfer: ""
  property string selectedPeer: ""
  property bool autoSelectPeer: false
  moduleName: "macondo.quickshare"
  property bool pendingReveal: false
  property bool selectionExpanded: true
  property bool settingsExpanded: false
  property bool pendingSelectionFocus: false
  property string effectsPreviewMode: "preparation"
  property string effectsPreviewTransport: "lan"
  property bool effectsPopupReady: false
  property var effectsRegisteredService: null
  function registerEffectsAnchor() {
    if (effectsRegisteredService && effectsRegisteredService.effectsAnchorWindow === popup)
      effectsRegisteredService.effectsAnchorWindow = null
    effectsRegisteredService = effectsPopupReady ? service : null
    if (effectsRegisteredService) effectsRegisteredService.effectsAnchorWindow = popup
  }
  onServiceChanged: registerEffectsAnchor()
  bar: service ? service.panelBar : null
  manageIpc: false
  readonly property color foreground: Color.popups.text
  readonly property color dim: Qt.darker(foreground, 1.4)
  readonly property string fontFamily: Style.font.family
  readonly property var devices: service ? service.devices : []
  readonly property bool connected: service && service.online
  readonly property bool enabledActions: connected && !service.busy
  readonly property var qrInvitation: service && service.qrSend ? service.qrSend : null
  readonly property bool qrSupported: !!service && service.qrSendSupported === true
  readonly property bool lanAvailable: root.connected && !!service.snapshot && !!service.snapshot.capabilities && !!service.snapshot.capabilities.lan
  readonly property var displayedTransfers: service ? service.transfers.slice().reverse().sort(function(a, b) {
    return root.transferPriority(a) - root.transferPriority(b)
  }) : []
  readonly property bool selectedPeerAvailable: hasPeer(selectedPeer)

  onDevicesChanged: updatePeerSelection()
  onDisplayedTransfersChanged: syncTransfers()
  Component.onCompleted: syncTransfers()
  onOpenedChanged: {
    if (!opened) stopPendingQr()
  }
  Component.onDestruction: stopPendingQr()

  function stopPendingQr() {
    // Also cancel a start still in flight; Service owns deferred cancellation.
    if (service && typeof service.stopQr === "function") service.stopQr()
  }

  ListModel { id: transferModel; dynamicRoles: true }
  function syncTransfers() {
    // Updating rows preserves pressed buttons while progress snapshots arrive.
    for (var i = 0; i < displayedTransfers.length; i++) {
      var next = displayedTransfers[i]
      if (i >= transferModel.count || transferModel.get(i).transferId !== next.id) {
        var existing = -1
        for (var j = i + 1; j < transferModel.count; j++) {
          if (transferModel.get(j).transferId === next.id) { existing = j; break }
        }
        if (existing >= 0) transferModel.move(existing, i, 1)
        else transferModel.insert(i, { transferId: next.id, transferData: next })
      }
      transferModel.setProperty(i, "transferData", next)
    }
    if (transferModel.count > displayedTransfers.length)
      transferModel.remove(displayedTransfers.length, transferModel.count - displayedTransfers.length)
    if (pendingReveal) Qt.callLater(root.revealTransfer)
  }

  function hasPeer(peer) {
    return devices.some(function(device) { return device.id === peer })
  }
  function updatePeerSelection() {
    if (autoSelectPeer) selectedPeer = devices.length === 1 ? devices[0].id : ""
    else if (!hasPeer(selectedPeer)) selectedPeer = ""
  }
  function choosePeer(peer) {
    autoSelectPeer = false
    selectedPeer = peer
  }

  function transferPriority(transfer) {
    if (transfer.state === "waiting_acceptance") return transfer.id === highlightedTransfer ? 0 : 1
    if (!terminal(transfer.state)) return 2
    return transfer.id === highlightedTransfer ? 3 : 4
  }
  function revealTransfer() {
    if (!pendingReveal) return
    for (var i = 0; i < transferCards.count; i++) {
      var card = transferCards.itemAt(i)
      if (card && card.transferData.id === highlightedTransfer && card.height > 0) {
        var position = card.mapToItem(column, 0, 0)
        flick.contentY = Math.max(0, Math.min(position.y, flick.contentHeight - flick.height))
        pendingReveal = false
        return
      }
    }
  }
  function focusSelection() {
    if (!pendingSelectionFocus || pendingReveal) return
    flick.contentY = Math.max(0, Math.min(selectionSection.y, flick.contentHeight - flick.height))
    selectionHeader.forceActiveFocus()
    pendingSelectionFocus = false
  }

  function open(payload) {
    var request = payload
    if (typeof payload === "string") {
      try { request = payload ? JSON.parse(payload) : {} } catch (e) { request = {} }
    }
    var hasShare = request && Object.prototype.hasOwnProperty.call(request, "share_token")
    autoSelectPeer = !!hasShare
    if (hasShare && service) service.takeShare(request.share_token)
    if (hasShare) {
      selectedPeer = ""
      updatePeerSelection()
      selectionExpanded = true
    }
    var requestedTransfer = request && request.transfer_id ? String(request.transfer_id) : ""
    if (requestedTransfer !== "" || !hasShare) highlightedTransfer = requestedTransfer
    pendingReveal = requestedTransfer !== ""
    pendingSelectionFocus = !!hasShare && !pendingReveal
    if (!pendingReveal && !pendingSelectionFocus) flick.contentY = 0
    controller.show()
    if (service) service.refresh()
    if (pendingReveal) Qt.callLater(root.revealTransfer)
    else if (pendingSelectionFocus) Qt.callLater(root.focusSelection)
  }
  function tr(en, es) { return service ? service.tr(en, es) : en }
  function medium(value) {
    return value === "lan" ? tr("Local network", "Red local") : value === "bluetooth" ? "Bluetooth" : value === "hotspot" ? tr("Direct Wi-Fi / hotspot", "Wi-Fi directo / punto de acceso") : tr("Transport not confirmed", "Sin medio confirmado")
  }
  function stateLabel(value) {
    var labels = { connecting: tr("Connecting", "Conectando"), authenticating: tr("Verifying", "Verificando"), waiting_acceptance: tr("Waiting for approval", "Esperando autorización"), negotiating: tr("Negotiating connection", "Negociando conexión"), transferring: tr("Transferring", "Transfiriendo"), completed: tr("Completed", "Completada"), rejected: tr("Rejected", "Rechazada"), cancelled: tr("Cancelled", "Cancelada"), failed: tr("Failed", "Fallida") }
    return labels[value] || value
  }
  function terminal(value) { return ["completed", "rejected", "cancelled", "failed"].indexOf(value) !== -1 }
  function bytes(value) {
    var n = Number(value || 0)
    if (n < 1024) return n + " B"
    if (n < 1048576) return (n / 1024).toFixed(1) + " KiB"
    if (n < 1073741824) return (n / 1048576).toFixed(1) + " MiB"
    return (n / 1073741824).toFixed(2) + " GiB"
  }
  function fileLabel(file) { return typeof file === "string" ? file : String(file.name || file.file_name || file.path || tr("File", "Archivo")) }
  function fileName(path) { return path.slice(path.lastIndexOf("/") + 1) || path }

  component ActionButton: Ui.Button {
    focusable: true
    bordered: true
    foreground: root.foreground
    opacity: enabled ? 1 : 0.5
    Accessible.role: Accessible.Button
    Accessible.name: text
  }
  component BodyText: Text {
    color: root.foreground
    font.family: root.fontFamily
    font.pixelSize: Style.font.body
    textFormat: Text.PlainText
    wrapMode: Text.Wrap
    Layout.fillWidth: true
  }

  component SectionCard: Ui.BorderSurface {
    id: surface
    default property alias content: body.data
    Layout.fillWidth: true
    implicitHeight: body.implicitHeight + Style.space(28)
    color: Color.popups.background
    borderSpec: Border.controlSpec("normal", root.foreground, Color.accent)
    radius: Style.cornerRadius
    ColumnLayout {
      id: body
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.top: parent.top
      anchors.margins: Style.space(14)
      spacing: Style.space(10)
    }
  }
  component StatusBadge: Ui.BorderSurface {
    id: badge
    property string caption: ""
    property bool emphasized: false
    property bool muted: false
    implicitWidth: badgeText.implicitWidth + Style.space(14)
    implicitHeight: badgeText.implicitHeight + Style.space(8)
    color: emphasized ? Style.selectedFillFor(root.foreground, Color.accent) : Style.normalFillFor(root.foreground, Color.accent)
    borderSpec: Border.controlSpec(emphasized ? "selected" : "normal", root.foreground, Color.accent)
    radius: Style.cornerRadius
    opacity: muted ? 0.5 : 1
    Text {
      id: badgeText
      anchors.centerIn: parent
      text: badge.caption
      textFormat: Text.PlainText
      color: badge.emphasized ? Color.accent : root.dim
      font.family: root.fontFamily
      font.pixelSize: Style.font.caption
      font.bold: true
    }
  }
  component EmptyState: RowLayout {
    id: empty
    property string icon: ""
    property string title: ""
    property string description: ""
    Layout.fillWidth: true
    spacing: Style.space(14)
    BodyText { Layout.fillWidth: false; text: empty.icon; color: Color.accent; font.pixelSize: Style.font.display; Accessible.ignored: true }
    ColumnLayout {
      Layout.fillWidth: true
      spacing: Style.space(4)
      BodyText { text: empty.title; font.bold: true }
      BodyText { text: empty.description; color: root.dim; font.pixelSize: Style.font.bodySmall }
    }
  }

  Ui.KeyboardPanel {
    id: popup
    anchorItem: service ? service.panelAnchor : null
    bar: root.bar
    owner: root
    open: root.opened
    focusTarget: content
    centerOnBar: true
    contentWidth: fittedContentWidth(Style.space(520))
    contentHeight: fittedContentHeight(column.implicitHeight, Style.space(720))
    Component.onCompleted: {
      root.effectsPopupReady = true
      root.registerEffectsAnchor()
    }
    Component.onDestruction: {
      if (root.effectsRegisteredService) root.effectsRegisteredService.stopTransferEffectsPreview()
      root.effectsPopupReady = false
      root.registerEffectsAnchor()
    }

    FocusScope {
      id: content
      anchors.fill: parent
      Keys.onEscapePressed: root.close()
      Flickable {
        id: flick
        anchors.fill: parent
        contentWidth: width
        contentHeight: column.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        onContentHeightChanged: {
          if (root.pendingReveal) Qt.callLater(root.revealTransfer)
          else if (root.pendingSelectionFocus) Qt.callLater(root.focusSelection)
        }
        ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }
        ColumnLayout {
          id: column
          width: flick.width
          spacing: Style.space(16)
          Ui.PanelHero {
            Layout.fillWidth: true
            title: "Quick Share"
            meta: root.tr("Android file sharing", "Compartir archivos con Android") + (root.manifest && root.manifest.version ? " · v" + root.manifest.version : "")
            detail: root.connected ? root.tr("Ready", "Listo") : service && service.serviceDisabled ? root.tr("Service disabled", "Servicio desactivado") : service && !service.backendInstalled ? root.tr("Setup required", "Requiere instalación") : service && !service.backendCompatible ? root.tr("Service update required", "Requiere actualizar el servicio") : root.tr("Service unavailable", "Servicio no disponible")
            foreground: root.foreground
            iconOpacity: root.connected ? 1 : 0.5
            iconComponent: Ui.BorderSurface {
              implicitWidth: Style.space(48)
              implicitHeight: Style.space(48)
              color: Style.selectedFillFor(root.foreground, Color.accent)
              radius: Style.cornerRadius
              Text {
                anchors.centerIn: parent
                text: "󰒗"
                textFormat: Text.PlainText
                color: Color.accent
                font.family: root.fontFamily
                font.pixelSize: Style.font.heading
                Accessible.ignored: true
              }
            }
            trailingControl: ActionButton {
              bordered: false
              iconText: "󰅖"
              tooltipText: root.tr("Close panel · transfers keep running", "Cerrar panel · las transferencias continúan")
              Accessible.name: root.tr("Close panel", "Cerrar panel")
              onClicked: root.close()
            }
          }
          BodyText {
            text: "Powered by Jhonny Suarez - PuroDelphi"
            color: root.dim
            font.pixelSize: Style.font.bodySmall
            horizontalAlignment: Text.AlignHCenter
            Layout.topMargin: Style.spacing.controlGap
          }
          BodyText {
            visible: !root.connected
            text: service && service.setupBusy ? root.tr("Complete the maintenance action in the Omarchy terminal, then return here.", "Completa el mantenimiento en la terminal de Omarchy y vuelve aquí.") : service && service.serviceDisabled ? root.tr("The service is disabled. Install service enables it again without changing preferences or received files.", "El servicio está desactivado. Instalar servicio lo habilita de nuevo sin cambiar preferencias ni archivos recibidos.") : service && service.backendInstalled && !service.backendCompatible ? (service.minimumBackendVersion ? root.tr("Update the client and running service to version ", "Actualiza el cliente y el servicio activo a la versión ") + service.minimumBackendVersion + root.tr(" or newer. Install service opens the authorized upgrade terminal.", " o posterior. Instalar servicio abre la terminal de actualización autorizada.") : root.tr("Update the client and running service to the current stable version. Install service opens the authorized upgrade terminal.", "Actualiza el cliente y el servicio activo a la versión estable actual. Instalar servicio abre la terminal de actualización autorizada.")) : root.tr("Install service to set up or repair the backend. Sharing remains unavailable until the service connects.", "Instala el servicio para configurar o reparar el motor. Compartir no estará disponible hasta conectar el servicio.")
            color: root.dim
            font.pixelSize: Style.font.bodySmall
          }
          RowLayout {
            Layout.fillWidth: true
            spacing: Style.spacing.controlGap
            Ui.Dropdown {
              id: languagePicker
              Layout.preferredWidth: Style.space(118)
              showLabel: false
              value: service ? service.language : "en"
              options: [{ value: "en", label: "English" }, { value: "es", label: "Español" }]
              foreground: root.foreground
              enabled: !!service
              Accessible.name: root.tr("Interface language", "Idioma de la interfaz")
              onChanged: function(value) { service.setLanguage(value) }
            }
            ActionButton {
              Layout.fillWidth: true
              Layout.minimumWidth: 0
              iconText: service && service.discoverable ? "󰈈" : "󰈉"
              text: service && service.discoverable ? root.tr("Hide · ", "Ocultar · ") + service.remainingSeconds + " s" : root.tr("Receive for 120 s", "Recibir durante 120 s")
              tooltipText: root.tr("Make this computer visible to nearby devices temporarily", "Haz visible este equipo a dispositivos cercanos temporalmente")
              active: service && service.discoverable
              enabled: root.enabledActions
              onClicked: service.setDiscoverable(!service.discoverable)
            }
            ActionButton {
              iconText: "󰑐"
              tooltipText: root.tr("Refresh status and devices", "Actualizar estado y dispositivos")
              Accessible.name: tooltipText
              enabled: !!service
              onClicked: service.refresh()
            }
          }
          SectionCard {
            visible: !root.connected || root.settingsExpanded
            BodyText { text: root.tr("Service", "Servicio"); font.bold: true }
            BodyText {
              visible: service && service.setupMessage !== ""
              text: service ? service.setupMessage : ""
              font.pixelSize: Style.font.bodySmall
            }
            BodyText {
              text: service && service.activeTransfers ? root.tr("Finish or cancel active transfers before maintenance.", "Termina o cancela las transferencias activas antes del mantenimiento.") : root.tr("Maintenance opens an Omarchy terminal for authorization. The native plugin checkout stays intact.", "El mantenimiento abre una terminal de Omarchy para autorizarlo. El checkout Git del plugin permanece intacto.")
              color: root.dim
              font.pixelSize: Style.font.bodySmall
            }
            ActionButton {
              Layout.fillWidth: true
              text: root.tr("Install service", "Instalar servicio")
              enabled: service && !service.busy && !service.activeTransfers
              onClicked: service.runSetup("install")
            }
            ActionButton {
              Layout.fillWidth: true
              text: root.tr("Update service", "Actualizar servicio")
              enabled: service && service.backendInstalled && !service.busy && !service.activeTransfers
              onClicked: service.runSetup("update")
            }
            ActionButton {
              Layout.fillWidth: true
              text: root.tr("Remove service", "Retirar servicio")
              enabled: service && service.backendInstalled && !service.busy && !service.activeTransfers
              onClicked: service.runSetup("remove")
            }
          }
          Flow {
            Layout.fillWidth: true
            spacing: Style.space(6)
            Repeater {
              model: [{ key: "lan", name: "LAN" }, { key: "bluetooth", name: "Bluetooth" }, { key: "hotspot", name: root.tr("Hotspot", "Punto de acceso") }]
              StatusBadge {
                required property var modelData
                readonly property bool available: root.connected && service.snapshot && service.snapshot.capabilities && service.snapshot.capabilities[modelData.key]
                caption: modelData.name + (available ? " 󰄬" : " —")
                muted: !available
                Accessible.name: modelData.name + (available ? root.tr(" available locally", " disponible localmente") : root.tr(" unavailable locally", " no disponible localmente"))
              }
            }
          }
          SectionCard {
            visible: service && service.errorText !== ""
            borderSpec: Border.controlSpec("normal", Color.urgent, Color.urgent)
            BodyText { text: root.tr("Attention required", "Revisa este problema"); color: Color.urgent; font.bold: true }
            BodyText { text: service ? service.errorText : ""; font.pixelSize: Style.font.bodySmall; wrapMode: Text.WrapAnywhere }
            ActionButton { text: root.tr("Dismiss", "Limpiar"); onClicked: service.clearErrors() }
          }
          SectionCard {
            id: selectionSection
            ActionButton {
              id: selectionHeader
              Layout.fillWidth: true
              bordered: false
              leftAlign: true
              iconText: root.selectionExpanded ? "󰅀" : "󰅂"
              text: root.tr("Send files", "Enviar archivos") + (service && service.selectedFiles.length > 0 ? " · " + service.selectedFiles.length : "")
              fontSize: Style.font.subtitle
              Accessible.description: root.selectionExpanded ? root.tr("Selection expanded", "Selección abierta") : root.tr("Selection collapsed", "Selección cerrada")
              onClicked: root.selectionExpanded = !root.selectionExpanded
            }
            ColumnLayout {
              visible: root.selectionExpanded
              Layout.fillWidth: true
              spacing: Style.space(10)
              Ui.PanelSectionHeader { text: root.tr("NEARBY DEVICES", "DISPOSITIVOS CERCANOS"); foreground: root.foreground }
              EmptyState {
                visible: root.devices.length === 0
                icon: "󰄜"
                title: root.tr("Find your Android", "Encuentra tu Android")
                description: root.tr("Open Quick Share → Receive on your phone and keep Bluetooth on.", "Abre Quick Share → Recibir en el teléfono y mantén Bluetooth encendido.")
              }
              Repeater {
                model: root.devices
                ActionButton {
                  required property var modelData
                  Layout.fillWidth: true
                  leftAlign: true
                  selected: root.selectedPeer === modelData.id
                  iconText: selected ? "󰄬" : "󰄜"
                  text: modelData.name + " · " + root.medium(modelData.transport)
                  enabled: root.connected
                  Accessible.description: selected ? root.tr("Selected destination", "Destino seleccionado") : root.tr("Select destination", "Seleccionar destino")
                  onClicked: root.choosePeer(modelData.id)
                }
              }
              RowLayout {
                Layout.fillWidth: true
                spacing: Style.spacing.controlGap
                ActionButton {
                  Layout.fillWidth: true
                  iconText: "󰉋"
                  text: service && service.picking ? root.tr("Choosing…", "Seleccionando…") : root.tr("Choose files…", "Elegir archivos…")
                  enabled: service && !service.picking
                  onClicked: service.pickFiles()
                }
                ActionButton {
                  text: root.tr("Clear", "Vaciar")
                  iconText: "󰆴"
                  enabled: service && service.selectedFiles.length > 0 && !service.busy
                  onClicked: service.clearSelectedFiles()
                }
              }
              EmptyState {
                visible: !service || service.selectedFiles.length === 0
                icon: "󰈙"
                title: root.tr("Your next transfer", "Tu próxima transferencia")
                description: root.tr("Choose one or more files, or use “Send via Quick Share” in your file manager.", "Elige uno o varios archivos, o usa «Enviar por Quick Share» en el navegador de archivos.")
              }
              Repeater {
                model: service ? service.selectedFiles : []
                ColumnLayout {
                  required property string modelData
                  Layout.fillWidth: true
                  spacing: Style.space(2)
                  BodyText {
                    text: root.fileName(parent.modelData)
                    font.bold: true
                    Accessible.name: text
                  }
                  BodyText {
                    text: parent.modelData
                    font.pixelSize: Style.font.caption
                    color: root.dim
                    Accessible.name: text
                    wrapMode: Text.WrapAnywhere
                  }
                }
              }
              ActionButton {
                Layout.fillWidth: true
                iconText: "󰒗"
                text: root.tr("Send", "Enviar") + (service && service.selectedFiles.length > 0 ? " · " + service.selectedFiles.length + (service.selectedFiles.length === 1 ? root.tr(" file", " archivo") : root.tr(" files", " archivos")) : "")
                active: enabled
                enabled: root.enabledActions && !root.qrInvitation && root.selectedPeerAvailable && service.selectedFiles.length > 0
                onClicked: service.send(root.selectedPeer)
              }
              ActionButton {
                Layout.fillWidth: true
                text: root.tr("Show QR", "Mostrar QR")
                tooltipText: root.tr("Send the selected files without choosing a nearby device", "Envía los archivos seleccionados sin elegir un dispositivo cercano")
                enabled: root.enabledActions && root.qrSupported && root.lanAvailable && !root.qrInvitation && service.selectedFiles.length > 0
                onClicked: service.showQr()
              }
              BodyText {
                visible: !root.qrInvitation
                text: !root.connected ? root.tr("Connect to the Quick Share service to show a QR code.", "Conecta con el servicio de Quick Share para mostrar un código QR.")
                  : !root.qrSupported ? root.tr("Update the service to use Android Quick Share QR sending.", "Actualiza el servicio para enviar con QR de Quick Share de Android.")
                  : !root.lanAvailable ? root.tr("QR sending requires an available local network. Connect both devices to the same network.", "El envío con QR requiere una red local disponible. Conecta ambos dispositivos a la misma red.")
                  : root.tr("Or show a QR code for Android Quick Share on the same local network. No nearby device selection is needed.", "O muestra un código QR para Quick Share de Android en la misma red local. No hace falta seleccionar un dispositivo cercano.")
                color: root.dim
                font.pixelSize: Style.font.bodySmall
              }
              ColumnLayout {
                id: qrColumn
                visible: !!root.qrInvitation
                Layout.fillWidth: true
                spacing: Style.space(10)
                BodyText {
                  text: root.tr("Scan with Android Quick Share", "Escanea con Quick Share de Android")
                  font.bold: true
                }
                Rectangle {
                  id: qrSurface
                  Layout.alignment: Qt.AlignHCenter
                  Layout.preferredWidth: Math.min(Style.space(248), qrColumn.width)
                  Layout.preferredHeight: width
                  color: "#ffffff"
                  Image {
                    anchors.fill: parent
                    anchors.margins: Style.space(12)
                    source: root.qrInvitation ? root.qrInvitation.image : ""
                    fillMode: Image.PreserveAspectFit
                    smooth: false
                    Accessible.role: Accessible.Graphic
                    Accessible.name: root.tr("Quick Share invitation QR code", "Código QR de invitación de Quick Share")
                  }
                }
                BodyText {
                  text: root.qrInvitation ? String(root.qrInvitation.file_count) + (root.qrInvitation.file_count === 1 ? root.tr(" file", " archivo") : root.tr(" files", " archivos")) + " · " + root.tr("Expires in ", "Caduca en ") + Math.max(0, service.qrRemainingSeconds) + " s" : ""
                  horizontalAlignment: Text.AlignHCenter
                  font.bold: true
                }
                BodyText {
                  text: root.tr("Keep both devices on the same local network. Scan this code with Android Quick Share, then compare and confirm the PIN when requested.", "Mantén ambos dispositivos en la misma red local. Escanea este código con Quick Share de Android y después compara y confirma el PIN cuando se solicite.")
                  color: root.dim
                  font.pixelSize: Style.font.bodySmall
                }
                ActionButton {
                  Layout.fillWidth: true
                  text: root.tr("Close QR", "Cerrar QR")
                  onClicked: root.stopPendingQr()
                }
              }
              BodyText {
                text: root.tr("Compare the PIN on both devices before confirming.", "Compara el PIN en ambos dispositivos antes de confirmar.")
                color: root.dim
                font.pixelSize: Style.font.bodySmall
              }
            }
          }
          Ui.PanelSeparator { Layout.fillWidth: true; foreground: root.foreground }
          RowLayout {
            Layout.fillWidth: true
            Ui.PanelSectionHeader { text: root.tr("TRANSFERS", "TRANSFERENCIAS"); foreground: root.foreground; Layout.fillWidth: true }
            StatusBadge { caption: String(root.displayedTransfers.length) }
          }
          SectionCard {
            visible: root.displayedTransfers.length === 0
            EmptyState {
              icon: "󰁯"
              title: root.tr("No transfers yet", "Todavía no hay transferencias")
              description: root.tr("Progress and incoming requests will appear here.", "Aquí aparecerán el progreso y las solicitudes recibidas.")
            }
          }
          Repeater {
            id: transferCards
            model: transferModel
            onItemAdded: if (root.pendingReveal) Qt.callLater(root.revealTransfer)
            Ui.BorderSurface {
              id: card
              required property var transferData
              Layout.fillWidth: true
              implicitHeight: transferColumn.implicitHeight + Style.space(28)
              color: Color.popups.background
              borderSpec: Border.controlSpec(transferData.id === root.highlightedTransfer ? "selected" : "normal", root.foreground, Color.accent)
              radius: Style.cornerRadius
              ColumnLayout {
                id: transferColumn
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.top: parent.top
                anchors.margins: Style.space(14)
                spacing: Style.space(10)
                RowLayout {
                  Layout.fillWidth: true
                  BodyText {
                    Layout.fillWidth: false
                    text: card.transferData.state === "completed" ? "󰄬" : card.transferData.direction === "inbound" ? "󰇚" : "󰇘"
                    color: card.transferData.state === "failed" ? Color.urgent : Color.accent
                    font.pixelSize: Style.font.heading
                    Accessible.ignored: true
                  }
                  BodyText {
                    text: (card.transferData.direction === "inbound" ? root.tr("From: ", "De: ") : root.tr("To: ", "Para: ")) + card.transferData.device_name
                    font.bold: true
                    Accessible.name: text
                  }
                }
                Flow {
                  Layout.fillWidth: true
                  spacing: Style.space(6)
                  StatusBadge { caption: root.stateLabel(card.transferData.state); emphasized: card.transferData.state === "waiting_acceptance" || card.transferData.state === "completed" }
                  StatusBadge { caption: root.medium(card.transferData.transport) }
                }
                Ui.BorderSurface {
                  visible: !!card.transferData.pin_code && !root.terminal(card.transferData.state)
                  Layout.fillWidth: true
                  implicitHeight: pinColumn.implicitHeight + Style.space(16)
                  color: Style.selectedFillFor(root.foreground, Color.accent)
                  radius: Style.cornerRadius
                  ColumnLayout {
                    id: pinColumn
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: parent.top
                    anchors.margins: Style.space(8)
                    spacing: Style.space(4)
                    Ui.PanelSectionHeader { text: root.tr("VERIFY PIN", "VERIFICAR PIN"); foreground: root.foreground }
                    BodyText {
                      text: card.transferData.pin_code || ""
                      font.pixelSize: Style.font.title
                      color: Color.accent
                      font.letterSpacing: Style.space(3)
                      font.bold: true
                      Accessible.name: "PIN: " + text
                    }
                    BodyText {
                      text: root.tr("Check that this PIN matches the one on Android. If it does not match, reject the request.", "Comprueba que este PIN coincide con el de Android. Si no coincide, rechaza la solicitud.")
                      font.pixelSize: Style.font.bodySmall
                    }
                  }
                }
                Repeater {
                  model: card.transferData.files
                  BodyText { required property var modelData; text: root.fileLabel(modelData); font.pixelSize: Style.font.bodySmall; wrapMode: Text.WrapAnywhere; Accessible.name: text }
                }
                ProgressBar {
                  visible: card.transferData.state === "transferring" || card.transferData.state === "completed" || Number(card.transferData.transferred_bytes) > 0
                  Layout.fillWidth: true
                  from: 0
                  to: Math.max(1, Number(card.transferData.total_bytes))
                  value: Number(card.transferData.transferred_bytes)
                  indeterminate: false
                  background: Rectangle { implicitHeight: Style.space(6); color: Style.normalFillFor(root.foreground, Color.accent); radius: Style.cornerRadius }
                  contentItem: Item {
                    implicitHeight: Style.space(6)
                    Rectangle { width: parent.width * Math.min(1, Number(card.transferData.transferred_bytes) / Math.max(1, Number(card.transferData.total_bytes))); height: parent.height; radius: Style.cornerRadius; color: Style.selectedStateColor(root.foreground, Color.accent) }
                  }
                }
                RowLayout {
                  Layout.fillWidth: true
                  BodyText { text: root.bytes(card.transferData.transferred_bytes) + " / " + root.bytes(card.transferData.total_bytes); color: root.dim; font.pixelSize: Style.font.caption }
                  BodyText {
                    Layout.fillWidth: false
                    visible: Number(card.transferData.total_bytes) > 0 && (card.transferData.state === "transferring" || card.transferData.state === "completed")
                    text: Math.min(100, Math.floor(Number(card.transferData.transferred_bytes) * 100 / Math.max(1, Number(card.transferData.total_bytes)))) + "%"
                    color: Color.accent
                    font.pixelSize: Style.font.caption
                  }
                }
                BodyText { visible: !!card.transferData.error; text: card.transferData.error || ""; color: Color.urgent }
                ColumnLayout {
                  visible: !root.terminal(card.transferData.state)
                  Layout.fillWidth: true
                  spacing: Style.spacing.controlGap
                  ActionButton {
                    Layout.fillWidth: true
                    text: card.transferData.pin_code ? (card.transferData.direction === "outbound" ? root.tr("Confirm matching PIN", "Confirmar que el PIN coincide") : root.tr("PIN matches · accept", "El PIN coincide · aceptar")) : root.tr("Accept transfer", "Aceptar transferencia")
                    active: enabled
                    visible: card.transferData.state === "waiting_acceptance"
                    enabled: root.enabledActions
                    onClicked: service.respond(card.transferData.id, true)
                  }
                  RowLayout {
                    Layout.fillWidth: true
                    spacing: Style.spacing.controlGap
                    ActionButton {
                      Layout.fillWidth: true
                      text: root.tr("Reject", "Rechazar")
                      visible: card.transferData.state === "waiting_acceptance"
                      enabled: root.enabledActions
                      onClicked: service.respond(card.transferData.id, false)
                    }
                    ActionButton {
                      Layout.fillWidth: true
                      text: service && service.cancellationRequested(card.transferData.id) ? root.tr("Cancelling…", "Cancelando…") : root.tr("Cancel", "Cancelar")
                      enabled: root.connected && !service.cancellationRequested(card.transferData.id)
                      onClicked: service.cancel(card.transferData.id)
                    }
                  }
                }
              }
            }
          }
          Ui.PanelSeparator { Layout.fillWidth: true; foreground: root.foreground }
          ActionButton {
            Layout.fillWidth: true
            bordered: false
            leftAlign: true
            iconText: "󰒓"
            text: root.tr("Settings", "Opciones")
            fontSize: Style.font.subtitle
            Accessible.description: root.settingsExpanded ? root.tr("Settings expanded", "Opciones abiertas") : root.tr("Settings collapsed", "Opciones cerradas")
            onClicked: root.settingsExpanded = !root.settingsExpanded
          }
          ColumnLayout {
            visible: root.settingsExpanded
            Layout.fillWidth: true
            spacing: Style.space(10)
            SectionCard {
              Ui.PanelSectionHeader { text: root.tr("TRANSFER EFFECTS", "EFECTOS DE TRANSFERENCIA"); foreground: root.foreground }
              Ui.Toggle {
                Layout.fillWidth: true
                label: root.tr("Animated transfer companion", "Acompañante de transferencia animado")
                description: root.tr("Optional click-through effects around this card, or a compact companion when closed. Off by default.", "Efectos opcionales sin interceptar clics alrededor del panel, o un acompañante compacto al cerrarlo. Desactivados por defecto.")
                foreground: root.foreground
                checked: !!service && service.transferEffectsEnabled
                enabled: !!service
                onClicked: service.setTransferEffectsEnabled(!checked)
              }
              BodyText {
                text: service && service.online && service.activeTransfers
                  ? root.tr("Live transfers take priority; preview is paused.", "Las transferencias reales tienen prioridad; la vista previa está pausada.")
                  : root.tr("UI preview only · 8 seconds · no files sent, PIN or network actions.", "Solo vista previa visual · 8 segundos · sin enviar archivos, PIN ni acciones de red.")
                color: root.dim
                font.pixelSize: Style.font.bodySmall
              }
              Ui.Dropdown {
                Layout.fillWidth: true
                label: root.tr("Preview activity", "Actividad de vista previa")
                value: root.effectsPreviewMode
                options: [
                  { value: "preparation", label: root.tr("Preparation / connection", "Preparación / conexión") },
                  { value: "send", label: root.tr("Sending files", "Enviando archivos") },
                  { value: "receive", label: root.tr("Receiving files", "Recibiendo archivos") }
                ]
                foreground: root.foreground
                enabled: !!service && service.transferEffectsEnabled
                Accessible.name: root.tr("Preview activity", "Actividad de vista previa")
                onChanged: function(value) { root.effectsPreviewMode = value }
              }
              Ui.Dropdown {
                Layout.fillWidth: true
                label: root.tr("Preview transport", "Medio de vista previa")
                value: root.effectsPreviewTransport
                options: [
                  { value: "lan", label: root.tr("Wi-Fi / LAN", "Wi-Fi / red local") },
                  { value: "bluetooth", label: "Bluetooth" },
                  { value: "hotspot", label: root.tr("Direct Wi-Fi / hotspot", "Wi-Fi directo / punto de acceso") },
                  { value: "unknown", label: root.tr("Transport not confirmed", "Sin medio confirmado") }
                ]
                foreground: root.foreground
                enabled: !!service && service.transferEffectsEnabled
                Accessible.name: root.tr("Preview transport", "Medio de vista previa")
                onChanged: function(value) { root.effectsPreviewTransport = value }
              }
              RowLayout {
                Layout.fillWidth: true
                spacing: Style.spacing.controlGap
                ActionButton {
                  Layout.fillWidth: true
                  text: service && service.effectsPreview ? root.tr("Restart preview", "Reiniciar vista previa") : root.tr("Preview · 8 s", "Vista previa · 8 s")
                  enabled: !!service && service.transferEffectsEnabled && !(service.online && service.activeTransfers)
                  onClicked: service.previewTransferEffects(
                    root.effectsPreviewMode === "receive" ? "inbound" : "outbound",
                    root.effectsPreviewMode === "preparation" ? "connecting" : "transferring",
                    root.effectsPreviewTransport === "unknown" ? null : root.effectsPreviewTransport)
                }
                ActionButton {
                  text: root.tr("Stop", "Detener")
                  enabled: !!service && service.effectsPreview !== null
                  onClicked: service.stopTransferEffectsPreview()
                }
              }
            }
            Ui.Toggle {
              Layout.fillWidth: true
              label: root.tr("Allow interrupting my Wi-Fi", "Permitir interrumpir mi Wi-Fi")
              description: root.tr("Allow the service to try a direct connection. Android compatibility is not guaranteed.", "Autoriza al motor a intentar una conexión directa. No garantiza compatibilidad con Android.")
              foreground: root.foreground
              checked: service && service.snapshot ? service.snapshot.network_consent : false
              enabled: root.enabledActions
              opacity: enabled ? 1 : 0.5
              onClicked: service.consent(!checked)
            }
            Ui.PanelSectionHeader { text: root.tr("DOWNLOAD FOLDER", "CARPETA DE DESCARGAS"); foreground: root.foreground }
            BodyText {
              text: service && service.snapshot ? service.snapshot.download_directory : root.tr("No data", "Sin datos")
              color: root.dim
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WrapAnywhere
            }
            ActionButton {
              Layout.fillWidth: true
              text: service && service.pickingDirectory ? root.tr("Choosing folder…", "Eligiendo carpeta…") : root.tr("Choose folder…", "Elegir carpeta…")
              enabled: root.enabledActions && service.directoryPickerSupported && !service.pickingDirectory
              onClicked: { service.pickDownloadDirectory(); root.close() }
            }
            BodyText {
              visible: root.connected && !service.directoryPickerSupported
              text: root.tr("Update service to choose a download folder.", "Actualiza el servicio para elegir la carpeta de descargas.")
              color: root.dim
              font.pixelSize: Style.font.bodySmall
            }
          }
        }
      }
    }
  }
}
