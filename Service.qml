import QtQuick
import Quickshell
import Quickshell.Io

Item {
  id: root
  property var shell: null
  property QtObject panelBar: null
  property Item panelAnchor: null
  property var effectsAnchorWindow: null
  property var effectsPreview: null
  property bool savedTransferEffects: false
  property bool effectsPreferenceLoaded: false
  property bool effectsPreferenceWriting: false
  property var effectsPreferenceIssue: null
  readonly property bool transferEffectsEnabled: typeof entrySettings.transferEffectsEnabled === "boolean" ? entrySettings.transferEffectsEnabled : savedTransferEffects
  readonly property var transferEffectsRenderer: effectsLoader.item
  readonly property string effectsPreferenceError: effectsPreferenceIssue ? tr(effectsPreferenceIssue.en, effectsPreferenceIssue.es) + (effectsPreferenceIssue.detail ? "\n" + effectsPreferenceIssue.detail : "") : ""
  property var manifest: null
  property var snapshot: null
  property bool online: false
  property int generation: 0
  property string lastSnapshotInstanceId: ""
  property double lastSnapshotRevision: -1
  property int retrySeconds: 1
  property var cancellingTransfers: ({})
  property int pendingCommands: 0
  property bool picking: false
  property bool pickingDirectory: false
  property var selectedFiles: []
  property int selectionGeneration: 0
  property var pendingShare: null
  property bool qrPendingStart: false
  property bool qrCancelAfterStart: false
  property string qrStoppingId: ""
  property var localIssue: null
  readonly property string localError: localIssue ? tr(localIssue.en, localIssue.es) + (localIssue.detail ? "\n" + localIssue.detail : "") : ""
  property var entrySettings: ({})
  readonly property string language: entrySettings.language === "es" ? "es" : entrySettings.language === "en" ? "en" : savedLanguage
  property string watchError: ""
  property string savedLanguage: "en"
  property bool backendInstalled: false
  property bool backendClientCompatible: false
  property bool runtimeCompatible: false
  readonly property bool backendCompatible: backendClientCompatible && runtimeCompatible
  property string minimumBackendVersion: ""
  property var runtimeIssue: null
  readonly property string runtimeError: runtimeIssue ? tr(runtimeIssue.en, runtimeIssue.es) + (runtimeIssue.detail ? "\n" + runtimeIssue.detail : "") : ""
  property string backendBinary: ""
  property bool serviceDisabled: false
  property bool disabledLoaded: false
  property double setupAttempt: 0
  property var setupNotice: null
  readonly property bool setupBusy: setupAttempt > 0
  readonly property string setupMessage: setupNotice ? tr(setupNotice.en, setupNotice.es) : ""
  readonly property string configHome: Quickshell.env("XDG_CONFIG_HOME") || Quickshell.env("HOME") + "/.config"
  readonly property string serviceState: (Quickshell.env("XDG_STATE_HOME") || Quickshell.env("HOME") + "/.local/state") + "/omarchy-quickshare/service"
  readonly property string setupScript: decodeURIComponent(String(Qt.resolvedUrl("scripts/setup-service.sh")).replace(/^file:\/\//, ""))
  readonly property string serviceCommonScript: decodeURIComponent(String(Qt.resolvedUrl("scripts/service-common.sh")).replace(/^file:\/\//, ""))
  readonly property bool activeTransfers: transfers.some(function(transfer) {
    return ["completed", "rejected", "cancelled", "failed"].indexOf(transfer.state) === -1
  })
  property string dismissedBackendErrors: ""
  property double now: Date.now() / 1000
  readonly property bool busy: pendingCommands > 0 || setupBusy
  readonly property var devices: snapshot ? snapshot.devices : []
  readonly property var transfers: snapshot ? snapshot.transfers : []
  readonly property bool qrSendSupported: online && !!snapshot && snapshot.qr_send_supported === true
  readonly property var qrSend: online && !qrCancelAfterStart && snapshot && snapshot.qr_send
    && snapshot.qr_send.id !== qrStoppingId && snapshot.qr_send.expires_at > now ? snapshot.qr_send : null
  readonly property int qrRemainingSeconds: qrSend ? Math.max(0, Math.ceil(qrSend.expires_at - now)) : 0
  onSelectedFilesChanged: stopQr()
  readonly property bool directoryPickerSupported: online && runtimeCompatible && !!snapshot && snapshot.directory_picker_supported === true
  readonly property bool discoverable: online && snapshot && snapshot.visible &&
    (snapshot.visibility_expires_at === null || snapshot.visibility_expires_at > now)
  readonly property int remainingSeconds: discoverable && snapshot.visibility_expires_at !== null
    ? Math.max(0, Math.ceil(snapshot.visibility_expires_at - now)) : 0
  readonly property string backendErrors: snapshot ? (snapshot.errors || []).map(function(e) {
    return typeof e === "string" ? e : JSON.stringify(e)
  }).join("\n") : ""
  readonly property string errorText: [localError, effectsPreferenceError, runtimeError, !online && backendInstalled && backendClientCompatible && !runtimeIssue && !serviceDisabled && !setupBusy ? (watchError || tr("Service unavailable. Use Install service to repair setup, or review its terminal output.", "Servicio no disponible. Usa Instalar servicio para reparar la instalación o revisa su terminal.")) : "", online ? watchError : "", backendErrors !== dismissedBackendErrors ? backendErrors : ""].filter(function(s) { return s.length > 0 }).join("\n")

  function tr(en, es) { return language === "es" ? es : en }
  function fail(en, es, detail) { localIssue = { en: en, es: es, detail: detail || "" } }
  function loadSettings(raw) {
    try {
      var config = JSON.parse(raw)
      var layout = config.bar && config.bar.layout ? config.bar.layout : {}
      var entries = (layout.left || []).concat(layout.center || [], layout.right || [], config.plugins || [])
      entrySettings = entries.find(function(entry) { return entry && entry.id === "macondo.quickshare" }) || {}
      if (entrySettings.language === "en" || entrySettings.language === "es") persistLanguage(entrySettings.language)
      if (typeof entrySettings.transferEffectsEnabled === "boolean")
        persistTransferEffects(entrySettings.transferEffectsEnabled)
    } catch (e) {
      fail("Cannot read the language preference.", "No se pudo leer la preferencia de idioma.", e.message)
    }
  }
  function persistLanguage(value) {
    if (value === savedLanguage) return
    var task = commandComponent.createObject(root, {
      command: ["/usr/bin/env", "mkdir", "-p", configHome + "/omarchy-quickshare"],
      purpose: "language", preference: value
    })
    pendingCommands++
    task.running = true
  }
  function setLanguage(value) {
    if (value !== "en" && value !== "es" || value === language) return
    var next = Object.assign({}, entrySettings, { language: value })
    if (!shell || !shell.updateEntryInline("macondo.quickshare", next)) {
      fail("Cannot save the language preference.", "No se pudo guardar la preferencia de idioma.")
      return
    }
    entrySettings = next
    persistLanguage(value)
  }
  function effectsPreferenceFailed(en, es, detail) {
    effectsPreferenceIssue = { en: en, es: es, detail: detail || "" }
  }
  function persistTransferEffects(value) {
    if (!effectsPreferenceLoaded || effectsPreferenceWriting || value === savedTransferEffects) return
    effectsPreferenceWriting = true
    var task = commandComponent.createObject(root, {
      command: ["/usr/bin/env", "mkdir", "-p", configHome + "/omarchy-quickshare"],
      purpose: "effects-preference"
    })
    pendingCommands++
    task.running = true
  }
  function setTransferEffectsEnabled(value) {
    if (typeof value !== "boolean") return
    if (!value) stopTransferEffectsPreview()
    var next = Object.assign({}, entrySettings, { transferEffectsEnabled: value })
    if (!shell || !shell.updateEntryInline("macondo.quickshare", next)) {
      effectsPreferenceFailed("Cannot save the animation preference.", "No se pudo guardar la preferencia de animaciones.")
      return
    }
    entrySettings = next
    if (effectsPreferenceLoaded && value === savedTransferEffects) effectsPreferenceIssue = null
    persistTransferEffects(value)
  }
  function previewTransferEffects(direction, state, transport) {
    if (!transferEffectsEnabled || online && activeTransfers) return false
    if (["outbound", "inbound"].indexOf(direction) === -1 ||
        ["connecting", "transferring"].indexOf(state) === -1 ||
        transport !== null && ["lan", "bluetooth", "hotspot"].indexOf(transport) === -1) return false
    // UI-only generic data: never copy a backend record, path, peer or PIN.
    effectsPreview = {
      direction: direction, state: state, transport: transport,
      files: [{ name: tr("Preview file", "Archivo de muestra"), size: 8388608 }],
      total_bytes: 8388608, transferred_bytes: state === "transferring" ? 3145728 : 0
    }
    effectsPreviewExpiry.restart()
    return true
  }
  function stopTransferEffectsPreview() {
    effectsPreviewExpiry.stop()
    effectsPreview = null
  }
  onTransferEffectsEnabledChanged: if (!transferEffectsEnabled) stopTransferEffectsPreview()
  onActiveTransfersChanged: if (online && activeTransfers) stopTransferEffectsPreview()
  onOnlineChanged: if (!online || activeTransfers) stopTransferEffectsPreview()
  function invalidateRuntime() {
    generation++
    online = false
    runtimeCompatible = false
    snapshot = null
    cancellingTransfers = ({})
    picking = false
    pickingDirectory = false
    qrPendingStart = false
    qrCancelAfterStart = false
    qrStoppingId = ""
  }
  function probeBackend() {
    if (backendProbe.running) return
    reconnect.stop()
    invalidateRuntime()
    runtimeIssue = null
    watch.running = false
    backendProbe.running = true
  }
  function runSetup(action) {
    if (["install", "update", "remove"].indexOf(action) === -1 || busy) return
    if (activeTransfers) {
      fail("Finish or cancel transfers before maintaining the service.", "Termina o cancela las transferencias antes de mantener el servicio.")
      return
    }
    setupAttempt = Date.now()
    setupNotice = { en: "Authorize maintenance in the Omarchy terminal. Preferences and received files are preserved.", es: "Autoriza el mantenimiento en la terminal de Omarchy. Se conservan preferencias y archivos recibidos." }
    reconnect.stop()
    watch.running = false
    invalidateRuntime()
    setupLauncher.command = ["omarchy", "launch", "tui", "--app-id=io.omarchy.quickshare.setup", "bash", setupScript, action, language]
    setupLauncher.running = true
  }
  function consumeSetup(raw) {
    try {
      var receipt = JSON.parse(raw)
      if (!setupBusy || receipt.time < setupAttempt) return
      setupAttempt = 0
      if (receipt.outcome === "installed")
        setupNotice = { en: "Service installed and running.", es: "Servicio instalado y activo." }
      else if (receipt.outcome === "removed")
        setupNotice = { en: "Backend and Nautilus action removed. The native plugin, preferences and files are preserved.", es: "Servicio y acción Nautilus retirados. Se conservan el plugin nativo, las preferencias y los archivos." }
      else if (receipt.outcome === "retained")
        setupNotice = { en: "Service disabled; Nautilus action removed. The system package is retained, not uninstalled.", es: "Servicio desactivado; acción Nautilus retirada. El paquete del sistema se conserva, no está desinstalado." }
      else
        setupNotice = { en: "Maintenance did not complete. Review the actual error in the terminal and retry after resolving it.", es: "El mantenimiento no terminó. Revisa el error real en la terminal y reintenta después de resolverlo." }
      disabledFile.reload()
      probeBackend()
    } catch (e) {
      fail("Cannot read the maintenance result.", "No se pudo leer el resultado de mantenimiento.", e.message)
    }
  }
  onServiceDisabledChanged: {
    if (serviceDisabled) { reconnect.stop(); watch.running = false; invalidateRuntime() }
    else if (!setupBusy) probeBackend()
  }

  function stableVersion(value) {
    return typeof value === "string" && /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/.test(value)
  }
  function runtimeVersionSupported(value) {
    if (!stableVersion(value) || !stableVersion(minimumBackendVersion)) return false
    var actual = value.split(".")
    var minimum = minimumBackendVersion.split(".")
    for (var i = 0; i < 3; i++) {
      if (actual[i].length !== minimum[i].length) return actual[i].length > minimum[i].length
      if (actual[i] !== minimum[i]) return actual[i] > minimum[i]
    }
    return true
  }
  function consume(line, sourceGeneration) {
    if (sourceGeneration !== undefined && sourceGeneration !== generation) return
    if (!line.trim() && sourceGeneration === undefined) return
    try {
      var next = JSON.parse(line)
      // Identity is checked before revision deduplication: an old/replayed record
      // cannot retain a previously trusted online state.
      if (!backendClientCompatible || backendProbe.running || serviceDisabled || setupBusy ||
          !runtimeVersionSupported(next && next.backend_version)) {
        invalidateRuntime()
        runtimeIssue = {
          en: "The running service must report a stable backend version of at least " + (minimumBackendVersion || "the required minimum") + ". Use Install service to upgrade and restart it.",
          es: "El servicio activo debe indicar una versión estable de al menos " + (minimumBackendVersion || "la mínima requerida") + ". Usa Instalar servicio para actualizarlo y reiniciarlo."
        }
        return
      }
      if (next.version !== 1 || typeof next.instance_id !== "string" || !Number.isSafeInteger(next.revision) ||
          !Array.isArray(next.devices) || !Array.isArray(next.transfers)) {
        invalidateRuntime()
        runtimeIssue = { en: "Incompatible snapshot format. Use Install service to repair it.", es: "Formato de snapshot incompatible. Usa Instalar servicio para repararlo." }
        return
      }
      if (next.instance_id === lastSnapshotInstanceId && next.revision <= lastSnapshotRevision) return
      var remaining = {}
      if (snapshot && next.instance_id === snapshot.instance_id) {
        next.transfers.forEach(function(transfer) {
          if (root.cancellingTransfers[transfer.id] &&
              ["completed", "rejected", "cancelled", "failed"].indexOf(transfer.state) === -1)
            remaining[transfer.id] = true
        })
      }
      cancellingTransfers = remaining
      snapshot = next
      lastSnapshotInstanceId = next.instance_id
      lastSnapshotRevision = next.revision
      runtimeCompatible = true
      online = true
      watchError = ""
      retrySeconds = 1
      runtimeIssue = null
      consumePendingShare()
    } catch (e) {
      invalidateRuntime()
      runtimeIssue = { en: "Cannot trust the running service state.", es: "No se puede confiar en el estado del servicio activo.", detail: e.message }
    }
  }
  function clearErrors() {
    localIssue = null
    watchError = ""
    dismissedBackendErrors = backendErrors
  }
  function invoke(args, kind, transferId, inputPayload, selectionRequest) {
    if (serviceDisabled || !backendInstalled) {
      fail("Install service before using this action.", "Instala el servicio antes de usar esta acción.")
      return
    }
    if (!backendClientCompatible) {
      fail("Update service before using this action.", "Actualiza el servicio antes de usar esta acción.")
      return
    }
    if (!online || !runtimeCompatible || backendProbe.running || setupBusy) {
      fail("Wait for a compatible running service before using this action. Use Install service to repair it.", "Espera a un servicio activo compatible antes de usar esta acción. Usa Instalar servicio para repararlo.")
      return
    }
    var task = commandComponent.createObject(root, {
      command: ["/usr/bin/env", backendBinary].concat(args), purpose: kind || "action",
      sourceGeneration: generation, transferId: transferId || "",
      selectionRequest: selectionRequest === undefined ? selectionGeneration : selectionRequest,
      stdinEnabled: inputPayload !== undefined,
      inputPayload: inputPayload === undefined ? "" : JSON.stringify(inputPayload)
    })
    if (kind !== "picker" && kind !== "directory-picker") pendingCommands++
    task.running = true
  }
  function refresh() {
    if (setupBusy && !setupLauncher.running) {
      setupAttempt = 0
      setupNotice = { en: "Review the maintenance terminal for the result. No success has been assumed.", es: "Revisa la terminal de mantenimiento para conocer el resultado. No se ha supuesto éxito." }
    }
    disabledFile.reload()
    probeBackend()
  }
  function setDiscoverable(enabled) { invoke(["visible", enabled ? "120" : "0"]) }
  function consent(enabled) { invoke(["network-consent", enabled ? "on" : "off"]) }
  function pickDownloadDirectory() {
    if (!online || busy || pickingDirectory || !directoryPickerSupported ||
        serviceDisabled || !backendInstalled || !backendCompatible) return
    pickingDirectory = true
    invoke(["pick-files", "--directory"], "directory-picker")
  }
  function setSelectedFiles(files) {
    if (!Array.isArray(files) || files.length === 0 ||
        files.some(function(path) { return typeof path !== "string" || path.length === 0 || path[0] !== "/" })) {
      fail("Choose one or more files with absolute paths.", "Selecciona uno o varios archivos con rutas absolutas.")
      return false
    }
    selectedFiles = files.slice()
    return true
  }
  function beginSelectionRequest(clear) {
    selectionGeneration++
    pendingShare = null
    if (clear) selectedFiles = []
    return selectionGeneration
  }
  function clearSelectedFiles() { beginSelectionRequest(true) }
  function takeShare(token) {
    var requestGeneration = beginSelectionRequest(true)
    if (typeof token !== "string" || token.length === 0) {
      fail("Cannot read the shared file selection.", "No se pudo leer la selección de archivos compartidos.")
      return
    }
    pendingShare = { token: token, selectionRequest: requestGeneration }
    probeBackend()
  }
  function consumePendingShare() {
    if (!online || !runtimeCompatible) return
    var request = pendingShare
    pendingShare = null
    if (!request || request.selectionRequest !== selectionGeneration) return
    invoke(["take-share", request.token], "take-share", "", undefined, request.selectionRequest)
  }
  function pickFiles() {
    if (!online || picking || serviceDisabled || !backendInstalled || !backendCompatible) return
    var requestGeneration = beginSelectionRequest(false)
    picking = true
    invoke(["pick-files"], "picker", "", undefined, requestGeneration)
  }
  function showQr() {
    if (!online || busy || qrPendingStart || !qrSendSupported || !backendInstalled || !backendCompatible ||
        selectedFiles.length === 0 || !snapshot.capabilities || !snapshot.capabilities.lan) return
    qrPendingStart = true
    qrCancelAfterStart = false
    invoke(["qr-send"], "qr-send", "", selectedFiles, selectionGeneration)
  }
  function stopQrId(id) {
    if (!id || id === qrStoppingId || !online || serviceDisabled || !backendInstalled || !backendCompatible) return
    qrStoppingId = id
    invoke(["qr-stop", id], "qr-stop", id)
  }
  function stopQr() {
    if (qrPendingStart) qrCancelAfterStart = true
    var pending = snapshot && snapshot.qr_send
    if (pending) stopQrId(pending.id)
  }
  function send(peer) {
    if (!online || busy || qrSend || qrPendingStart || !peer || selectedFiles.length === 0) return
    invoke(["send", peer], "send", "", selectedFiles)
  }
  function respond(id, accept) { invoke([accept ? "accept" : "reject", id]) }
  function cancellationRequested(id) { return !!cancellingTransfers[id] }
  function clearCancellationRequest(id) {
    var next = Object.assign({}, cancellingTransfers)
    delete next[id]
    cancellingTransfers = next
  }
  function cancel(id) {
    if (!online || cancellationRequested(id)) return
    cancellingTransfers = Object.assign({}, cancellingTransfers, { [id]: true })
    invoke(["cancel", id], "cancel", id)
  }
  function disconnected(message) {
    invalidateRuntime()
    if (!watchError && message) watchError = message
    if (backendInstalled && backendClientCompatible && !backendProbe.running && !serviceDisabled && !setupBusy && !reconnect.running) {
      reconnect.restart()
      retrySeconds = Math.min(30, retrySeconds * 2)
    }
  }

  Component.onCompleted: probeBackend()
  Component.onDestruction: {
    stopTransferEffectsPreview()
    stopQr()
    effectsAnchorWindow = null
  }
  Timer {
    id: effectsPreviewExpiry
    interval: 8000
    onTriggered: root.stopTransferEffectsPreview()
  }
  // A URL-loaded renderer avoids pulling qs.Commons into the native service
  // registry's static import graph. Its lifetime is independent of Panel.qml.
  Loader {
    id: effectsLoader
    source: Qt.resolvedUrl("TransferEffects.qml")
    onLoaded: {
      item.enabled = Qt.binding(function() { return root.transferEffectsEnabled })
      item.connected = Qt.binding(function() { return root.online || root.effectsPreview !== null })
      item.transfers = Qt.binding(function() { return root.online ? root.transfers : [] })
      item.cancellingTransfers = Qt.binding(function() { return root.cancellingTransfers })
      item.preview = Qt.binding(function() { return root.effectsPreview })
      item.anchorWindow = Qt.binding(function() { return root.effectsAnchorWindow })
      item.anchorItem = Qt.binding(function() { return root.panelAnchor })
      item.bar = Qt.binding(function() { return root.panelBar })
      item.language = Qt.binding(function() { return root.language })
    }
    onStatusChanged: if (status === Loader.Error)
      root.fail("Cannot load transfer animations.", "No se pudieron cargar las animaciones de transferencia.")
  }
  Timer {
    interval: 1000
    repeat: true
    running: true
    onTriggered: root.now = Date.now() / 1000
  }
  Timer {
    id: reconnect
    interval: root.retrySeconds * 1000
    onTriggered: root.probeBackend()
  }
  Timer {
    interval: 1000
    repeat: true
    running: root.setupBusy
    onTriggered: { setupReceipt.reload(); disabledFile.reload() }
  }
  Process {
    id: backendProbe
    command: ["bash", "-c", "source \"$2\" || exit 1; binary=''; if [[ -x /usr/bin/omarchy-quickshare ]]; then binary=/usr/bin/omarchy-quickshare; elif [[ -x \"$1/.local/bin/omarchy-quickshare\" ]]; then binary=\"$1/.local/bin/omarchy-quickshare\"; fi; compatible=false; if [[ -n $binary ]] && backend_supported \"$binary\"; then compatible=true; fi; jq -cn --arg binary \"$binary\" --argjson compatible \"$compatible\" --arg minimum \"$MINIMUM_BACKEND_VERSION\" '{binary:$binary,compatible:$compatible,minimum:$minimum}'", "bash", Quickshell.env("HOME"), root.serviceCommonScript]
    stdout: StdioCollector { id: backendOutput; waitForEnd: true }
    onExited: function(code) {
      var result = {}
      try { if (code === 0) result = JSON.parse(backendOutput.text) } catch (e) {}
      root.backendBinary = result.binary || ""
      root.backendInstalled = root.backendBinary !== ""
      root.minimumBackendVersion = typeof result.minimum === "string" ? result.minimum : ""
      root.backendClientCompatible = result.compatible === true && root.stableVersion(root.minimumBackendVersion)
      if (!root.backendClientCompatible) {
        root.invalidateRuntime()
        root.runtimeIssue = root.backendInstalled && root.stableVersion(root.minimumBackendVersion)
          ? { en: "Update the installed service to a stable version of at least " + root.minimumBackendVersion + ". Use Install service to upgrade and restart it.", es: "Actualiza el servicio instalado a una versión estable de al menos " + root.minimumBackendVersion + ". Usa Instalar servicio para actualizarlo y reiniciarlo." }
          : root.backendInstalled || code !== 0 ? { en: "Cannot verify the installed service. Use Install service to repair setup.", es: "No se pudo verificar el servicio instalado. Usa Instalar servicio para reparar la instalación." } : null
      }
      if (root.backendInstalled && root.backendClientCompatible && root.disabledLoaded && !root.serviceDisabled && !root.setupBusy && !watch.running) {
        watch.command = ["/usr/bin/env", root.backendBinary, "watch"]
        watch.running = true
      }
      if (root.online && root.runtimeCompatible) root.consumePendingShare()
    }
  }
  Process {
    id: setupLauncher
    onExited: function(code) {
      if (code !== 0) {
        root.setupAttempt = 0
        root.fail("Cannot open the Omarchy setup terminal.", "No se pudo abrir la terminal de instalación de Omarchy.")
        disabledFile.reload()
        root.probeBackend()
      }
    }
  }
  FileView {
    id: disabledFile
    path: root.serviceState + "/disabled"
    watchChanges: true
    printErrors: false
    onLoaded: { root.serviceDisabled = text().trim() === "disabled"; root.disabledLoaded = true; root.probeBackend() }
    onLoadFailed: { root.serviceDisabled = false; root.disabledLoaded = true; root.probeBackend() }
    onFileChanged: reload()
  }
  FileView {
    id: setupReceipt
    path: root.serviceState + "/setup-result.json"
    watchChanges: true
    printErrors: false
    onLoaded: root.consumeSetup(text())
    onFileChanged: reload()
  }
  FileView {
    id: languageFile
    path: root.configHome + "/omarchy-quickshare/language"
    atomicWrites: true
    watchChanges: true
    printErrors: false
    onLoaded: {
      root.savedLanguage = text().trim() === "es" ? "es" : "en"
      if (root.entrySettings.language === "en" || root.entrySettings.language === "es")
        root.persistLanguage(root.entrySettings.language)
    }
    onFileChanged: reload()
  }
  FileView {
    id: effectsFile
    path: root.configHome + "/omarchy-quickshare/transfer-effects"
    atomicWrites: true
    watchChanges: true
    printErrors: false
    onLoaded: {
      var preference = text().trim()
      root.savedTransferEffects = preference === "on"
      root.effectsPreferenceLoaded = true
      if (preference !== "on" && preference !== "off")
        root.effectsPreferenceFailed("Invalid saved animation preference.", "La preferencia guardada de animaciones no es válida.")
      if (typeof root.entrySettings.transferEffectsEnabled === "boolean")
        root.persistTransferEffects(root.entrySettings.transferEffectsEnabled)
    }
    onLoadFailed: function(error) {
      root.savedTransferEffects = false
      root.effectsPreferenceLoaded = true
      if (error !== FileViewError.FileNotFound)
        root.effectsPreferenceFailed("Cannot read the animation preference.", "No se pudo leer la preferencia de animaciones.", FileViewError.toString(error))
      if (typeof root.entrySettings.transferEffectsEnabled === "boolean")
        root.persistTransferEffects(root.entrySettings.transferEffectsEnabled)
    }
    onSaved: {
      root.effectsPreferenceWriting = false
      root.savedTransferEffects = text().trim() === "on"
      root.effectsPreferenceIssue = null
      if (typeof root.entrySettings.transferEffectsEnabled === "boolean")
        root.persistTransferEffects(root.entrySettings.transferEffectsEnabled)
    }
    onSaveFailed: function(error) {
      root.effectsPreferenceWriting = false
      root.effectsPreferenceFailed("Cannot persist the animation preference.", "No se pudo conservar la preferencia de animaciones.", FileViewError.toString(error))
    }
    onFileChanged: reload()
  }
  FileView {
    path: Quickshell.env("HOME") + "/.config/omarchy/shell.json"
    watchChanges: true
    printErrors: false
    onLoaded: root.loadSettings(text())
    onFileChanged: reload()
  }
  Process {
    id: watch
    // Absolute backend selection avoids a stale graphical-session PATH or a shadow binary.
    command: []
    onStarted: {
      root.invalidateRuntime()
      root.lastSnapshotInstanceId = ""
      root.lastSnapshotRevision = -1
    }
    stdout: SplitParser { onRead: line => root.consume(line) }
    stderr: SplitParser { onRead: line => { if (line.trim()) root.watchError = line.trim() } }
    onExited: function(exitCode, exitStatus) { root.disconnected("") }
  }
  Component {
    id: commandComponent
    Process {
      id: task
      property string purpose: "action"
      property int sourceGeneration: 0
      property string transferId: ""
      property string preference: ""
      property int selectionRequest: 0
      property string inputPayload: ""
      onStarted: {
        if (!stdinEnabled) return
        write(inputPayload + "\n")
        inputPayload = ""
        // Quickshell closes the write channel when stdinEnabled becomes false.
        stdinEnabled = false
      }
      stdout: StdioCollector { id: output; waitForEnd: true }
      stderr: StdioCollector { id: errors; waitForEnd: true }
      onExited: function(exitCode, exitStatus) { finish(exitCode, exitStatus) }
      function finish(exitCode, exitStatus) {
        if (purpose !== "picker" && purpose !== "directory-picker") root.pendingCommands--
        var operational = purpose !== "language" && purpose !== "effects-preference"
        if (operational && (sourceGeneration !== root.generation ||
            (purpose !== "snapshot" && (!root.online || !root.runtimeCompatible)))) {
          Qt.callLater(function() { task.destroy() })
          return
        }
        if (purpose === "picker") root.picking = false
        if (purpose === "qr-send") root.qrPendingStart = false
        if (purpose === "directory-picker") root.pickingDirectory = false
        if ((purpose === "picker" || purpose === "take-share") && selectionRequest !== root.selectionGeneration) {
          Qt.callLater(function() { task.destroy() })
          return
        }
        if (exitCode !== 0 || exitStatus !== 0) {
          if (purpose === "snapshot") root.invalidateRuntime()
          if (purpose === "cancel") root.clearCancellationRequest(transferId)
          if (purpose === "qr-send") root.qrCancelAfterStart = true
          if (purpose === "qr-stop" && root.qrStoppingId === transferId) root.qrStoppingId = ""
          if (purpose === "effects-preference") {
            root.effectsPreferenceWriting = false
            root.effectsPreferenceFailed("Cannot persist the animation preference.", "No se pudo conservar la preferencia de animaciones.", errors.text.trim())
          }
          root.fail("The command could not be completed (" + exitCode + ").", "El comando no se pudo completar (" + exitCode + ").", errors.text.trim())
        } else {
          if (errors.text.trim()) root.fail("Service details", "Detalles del servicio", errors.text.trim())
          if (purpose === "language") {
            try {
              languageFile.setText(preference + "\n")
              root.savedLanguage = preference
            } catch (e) {
              root.fail("Cannot persist the language preference.", "No se pudo conservar la preferencia de idioma.", e.message)
            }
          }
          if (purpose === "effects-preference") {
            try {
              // Use the current setting, not the value when mkdir started.
              effectsFile.setText(root.transferEffectsEnabled ? "on\n" : "off\n")
            } catch (e) {
              root.effectsPreferenceWriting = false
              root.effectsPreferenceFailed("Cannot persist the animation preference.", "No se pudo conservar la preferencia de animaciones.", e.message)
            }
          }
          if (purpose === "snapshot") root.consume(output.text, sourceGeneration)
          else if (purpose === "qr-send") {
            try {
              var invitation = JSON.parse(output.text)
              if (!invitation || typeof invitation.id !== "string") throw new Error("Invalid QR invitation")
              if (root.qrCancelAfterStart || selectionRequest !== root.selectionGeneration)
                root.stopQrId(invitation.id)
            } catch (e) { root.fail("Cannot read the QR invitation.", "No se pudo leer la invitación QR.", e.message) }
          }
          else if (purpose === "picker") {
            try {
              var files = JSON.parse(output.text)
              // An empty portal result means cancellation, not a new selection.
              if (!Array.isArray(files) || files.length > 0) root.setSelectedFiles(files)
            } catch (e) { root.fail("Cannot read the file picker result.", "No se pudo leer el resultado del selector de archivos.", e.message) }
          } else if (purpose === "directory-picker") {
            try {
              var directories = JSON.parse(output.text)
              if (!Array.isArray(directories) || directories.length > 1 ||
                  (directories.length === 1 && (typeof directories[0] !== "string" || directories[0][0] !== "/")))
                throw new Error("Expected one local folder or a cancelled selection")
              if (directories.length === 1 && root.online && sourceGeneration === root.generation)
                root.invoke(["download-dir"], "action", "", directories[0])
            } catch (e) { root.fail("Cannot read the folder picker result.", "No se pudo leer el resultado del selector de carpetas.", e.message) }
          } else if (purpose === "take-share") {
            try {
              root.setSelectedFiles(JSON.parse(output.text))
            } catch (e) { root.fail("Cannot read the shared file selection.", "No se pudo leer la selección de archivos compartidos.", e.message) }
          } else if (purpose === "send" && selectionRequest === root.selectionGeneration) root.clearSelectedFiles()
        }
        Qt.callLater(function() { task.destroy() })
      }
    }
  }
}
