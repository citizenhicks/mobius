import AppKit
import Foundation
import Sparkle

/// What the rail and Settings show; Rust reads it after each `status_changed` event.
struct UpdateStatus: Encodable, Equatable {
    enum Stage: String, Encodable {
        case idle, checking, available, downloading, extracting, installing
    }

    var supported = false
    var reason: String?
    var stage = Stage.idle
    var version: String?
    var notesUrl: String?
    var progress: Double?
    var upToDate = false
}

enum UpdateChoice { case install, dismiss }

@MainActor
protocol UpdateChecking: AnyObject {
    func checkForUpdates()
}

extension SPUUpdater: UpdateChecking {}

enum UpdateAvailability {
    static let development = "Updates are unavailable in development builds."
    static let unconfigured = "Updates are not configured for this build."

    /// Why Sparkle cannot update this bundle, or nil when it can.
    static func reason(bundleURL: URL, info: [String: Any]) -> String? {
        guard bundleURL.pathExtension == "app", info["CFBundleIdentifier"] is String,
            let feed = (info["SUFeedURL"] as? String).flatMap(URL.init(string:)),
            feed.scheme == "https"
        else { return development }
        guard let key = info["SUPublicEDKey"] as? String, Data(base64Encoded: key)?.count == 32
        else { return unconfigured }
        return nil
    }
}

/// Sparkle checks, downloads and installs; the app, not Sparkle's windows, shows each stage.
@MainActor
final class NativeUpdates {
    private let emit: (JSONValue) -> Void
    private(set) var status = UpdateStatus()
    private var updater: UpdateChecking?
    private var driver: UpdateDriver?
    private var foundReply: ((UpdateChoice) -> Void)?
    private var informational = false
    private var userInitiated = false
    private var expected: UInt64 = 0
    private var received: UInt64 = 0

    init(reason: String?, updater: UpdateChecking? = nil, emit: @escaping (JSONValue) -> Void) {
        self.emit = emit
        self.updater = updater
        status.supported = reason == nil
        status.reason = reason
    }

    static func live(emit: @escaping (JSONValue) -> Void) -> NativeUpdates {
        let bundle = Bundle.main
        let reason = UpdateAvailability.reason(
            bundleURL: bundle.bundleURL, info: bundle.infoDictionary ?? [:])
        let updates = NativeUpdates(reason: reason, emit: emit)
        if reason == nil { updates.startSparkle() }
        return updates
    }

    private func startSparkle() {
        let driver = UpdateDriver(owner: self)
        let updater = SPUUpdater(
            hostBundle: .main, applicationBundle: .main, userDriver: driver, delegate: nil)
        do {
            try updater.start()
            self.driver = driver
            self.updater = updater
            updater.checkForUpdatesInBackground()
        } catch {
            update {
                $0.supported = false
                $0.reason = error.localizedDescription
            }
        }
    }

    func checkNow() {
        guard status.supported, status.stage == .idle, let updater else { return }
        userInitiated = true
        update {
            $0.stage = .checking
            $0.upToDate = false
        }
        updater.checkForUpdates()
    }

    /// Downloads, installs and relaunches the available update.
    func install() {
        guard status.supported, status.stage == .available else { return }
        if informational {
            if let notes = status.notesUrl.flatMap(URL.init(string:)) {
                NSWorkspace.shared.open(notes)
            }
            return
        }
        guard let reply = foundReply else { return }
        foundReply = nil
        userInitiated = true
        update {
            $0.stage = .downloading
            $0.progress = 0
        }
        reply(.install)
    }

    func checkStarted() { update { $0.stage = .checking } }

    func found(
        version: String, notes: URL?, informational: Bool,
        reply: @escaping (UpdateChoice) -> Void
    ) {
        foundReply = reply
        self.informational = informational
        userInitiated = false
        update {
            $0.stage = .available
            $0.version = version
            $0.notesUrl = notes?.absoluteString
            $0.progress = nil
            $0.upToDate = false
        }
    }

    func notFound() {
        userInitiated = false
        update {
            $0 = UpdateStatus(supported: true)
            $0.upToDate = true
        }
    }

    func downloadStarted() {
        expected = 0
        received = 0
        update {
            $0.stage = .downloading
            $0.progress = 0
        }
    }

    func expectedLength(_ length: UInt64) {
        expected = length
        received = 0
    }

    func receivedData(_ length: UInt64) {
        received += length
        guard expected > 0 else { return }
        let fraction = min(Double(received) / Double(expected), 1)
        update { $0.progress = (fraction * 100).rounded(.down) / 100 }
    }

    func extracting(_ progress: Double?) {
        update {
            $0.stage = .extracting
            $0.progress = progress.map { ($0 * 100).rounded(.down) / 100 }
        }
    }

    /// The user already chose to install, so the downloaded update installs at once.
    func ready(reply: @escaping (UpdateChoice) -> Void) {
        installing()
        reply(.install)
    }

    func installing() {
        update {
            $0.stage = .installing
            $0.progress = nil
        }
    }

    /// Background check failures stay quiet; anything the user started becomes a toast.
    func failed(_ message: String) {
        let visible = userInitiated || ![.idle, .checking].contains(status.stage)
        reset()
        if visible {
            emit(.error(message))
        }
    }

    /// Sparkle ended its session; an update the user never answered is found again later.
    func dismissed() {
        guard status.stage != .idle else { return }
        reset()
    }

    private func reset() {
        foundReply = nil
        informational = false
        userInitiated = false
        update { $0 = UpdateStatus(supported: true) }
    }

    private func update(_ change: (inout UpdateStatus) -> Void) {
        var next = status
        change(&next)
        guard next != status else { return }
        status = next
        emit(.statusChanged)
    }

    func encodedStatus() -> String {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        guard let data = try? encoder.encode(status) else { return "{}" }
        return String(decoding: data, as: UTF8.self)
    }
}

/// Adapts Sparkle's user driver callbacks to `NativeUpdates`.
@MainActor
final class UpdateDriver: NSObject, SPUUserDriver {
    private weak var owner: NativeUpdates?

    init(owner: NativeUpdates) { self.owner = owner }

    func show(
        _ request: SPUUpdatePermissionRequest,
        reply: @escaping (SUUpdatePermissionResponse) -> Void
    ) {
        reply(SUUpdatePermissionResponse(automaticUpdateChecks: true, sendSystemProfile: false))
    }

    func showUserInitiatedUpdateCheck(cancellation: @escaping () -> Void) {
        owner?.checkStarted()
    }

    func showUpdateFound(
        with appcastItem: SUAppcastItem, state: SPUUserUpdateState,
        reply: @escaping (SPUUserUpdateChoice) -> Void
    ) {
        let notes =
            appcastItem.fullReleaseNotesURL ?? appcastItem.releaseNotesURL ?? appcastItem.infoURL
        owner?.found(
            version: appcastItem.displayVersionString, notes: notes,
            informational: appcastItem.isInformationOnlyUpdate
        ) { choice in
            reply(choice == .install ? .install : .dismiss)
        }
    }

    func showUpdateReleaseNotes(with downloadData: SPUDownloadData) {}

    func showUpdateReleaseNotesFailedToDownloadWithError(_ error: any Error) {}

    func showUpdateNotFoundWithError(
        _ error: any Error, acknowledgement: @escaping () -> Void
    ) {
        owner?.notFound()
        acknowledgement()
    }

    func showUpdaterError(_ error: any Error, acknowledgement: @escaping () -> Void) {
        owner?.failed(error.localizedDescription)
        acknowledgement()
    }

    func showDownloadInitiated(cancellation: @escaping () -> Void) {
        owner?.downloadStarted()
    }

    func showDownloadDidReceiveExpectedContentLength(_ expectedContentLength: UInt64) {
        owner?.expectedLength(expectedContentLength)
    }

    func showDownloadDidReceiveData(ofLength length: UInt64) {
        owner?.receivedData(length)
    }

    func showDownloadDidStartExtractingUpdate() { owner?.extracting(nil) }

    func showExtractionReceivedProgress(_ progress: Double) { owner?.extracting(progress) }

    func showReady(toInstallAndRelaunch reply: @escaping (SPUUserUpdateChoice) -> Void) {
        guard let owner else { return reply(.dismiss) }
        owner.ready { reply($0 == .install ? .install : .dismiss) }
    }

    func showInstallingUpdate(
        withApplicationTerminated applicationTerminated: Bool,
        retryTerminatingApplication: @escaping () -> Void
    ) {
        owner?.installing()
    }

    func showUpdateInstalledAndRelaunched(
        _ relaunched: Bool, acknowledgement: @escaping () -> Void
    ) {
        owner?.dismissed()
        acknowledgement()
    }

    func dismissUpdateInstallation() { owner?.dismissed() }
}

@_cdecl("mobius_native_update_status")
public func nativeUpdateStatus() -> UnsafeMutablePointer<CChar>? {
    strdup(MainActor.assumeIsolated { NativeBridge.shared.updates.encodedStatus() })
}

/// 0 checks now; 1 downloads, installs and relaunches the available update.
@_cdecl("mobius_native_update")
public func nativeUpdate(_ action: UInt8) {
    MainActor.assumeIsolated {
        let updates = NativeBridge.shared.updates
        switch action {
        case 0: updates.checkNow()
        case 1: updates.install()
        default: break
        }
    }
}
