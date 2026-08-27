import Foundation
import SwiftUI
import XCTest

@MainActor
extension AppModelTests {
    func testAppLockAuthenticatesBeforePersistingAndKeepsWorkspaceDraftInMemory() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        var method = AppLockAuthenticationMethod.unavailable
        var results = [false, true, true, true]
        let authenticator = AppLockAuthenticator(
            method: { method },
            authenticate: { _ in results.removeFirst() }
        )
        let app = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            appLockAuthenticator: authenticator
        )
        await app.appDidBecomeActive()

        await app.setAppLockEnabled(true)
        XCTAssertFalse(app.appLockEnabled)
        XCTAssertFalse(defaults.bool(forKey: "app-lock-enabled"))

        method = .faceID
        await app.setAppLockEnabled(true)
        XCTAssertFalse(app.appLockEnabled)
        XCTAssertEqual(app.appLockAuthenticationMethod.settingTitle, "Require Face ID")

        await app.setAppLockEnabled(true)
        XCTAssertTrue(app.appLockEnabled)
        XCTAssertFalse(app.isAppLocked)
        XCTAssertTrue(defaults.bool(forKey: "app-lock-enabled"))

        let relaunched = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            appLockAuthenticator: authenticator
        )
        XCTAssertTrue(relaunched.isAppLocked)
        await relaunched.appDidBecomeActive()
        XCTAssertFalse(relaunched.isAppLocked)
        let draftID = UUID()
        relaunched.textFilePreview = TextFilePreview(
            id: draftID,
            name: "New File",
            contents: "",
            workspaceSessionID: "chat-1",
            workspacePath: ""
        )
        relaunched.updateWorkspaceFileDraft(id: draftID, path: ".env")
        relaunched.updateWorkspaceFileDraft(id: draftID, contents: "TOKEN=unsaved\n")
        relaunched.appDidEnterBackground()
        XCTAssertTrue(relaunched.isAppLocked)
        XCTAssertEqual(relaunched.textFilePreview?.workspacePath, ".env")
        XCTAssertEqual(relaunched.textFilePreview?.contents, "TOKEN=unsaved\n")
        await relaunched.appDidBecomeActive()
        XCTAssertFalse(relaunched.isAppLocked)
        XCTAssertEqual(relaunched.textFilePreview?.contents, "TOKEN=unsaved\n")

        let freshLaunch = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            appLockAuthenticator: authenticator
        )
        XCTAssertNil(freshLaunch.textFilePreview)
        XCTAssertTrue(results.isEmpty)
    }

    func testThemeUsesTheInjectedDefaults() throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        defaults.set(ThemePreference.lightsOut.rawValue, forKey: "theme")
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults
        )

        XCTAssertEqual(model.theme, .lightsOut)
        model.setTheme(.light)
        XCTAssertEqual(defaults.string(forKey: "theme"), ThemePreference.light.rawValue)
    }

    func testLightsOutUsesBlackCanvasWithDarkPalette() {
        let dark = MobiusPalette(.dark)
        let lightsOut = MobiusPalette(.light, lightsOut: true)

        XCTAssertEqual(lightsOut.canvas, .black)
        XCTAssertEqual(
            [
                lightsOut.recessed, lightsOut.panel, lightsOut.raised, lightsOut.line,
                lightsOut.accent, lightsOut.accentFill, lightsOut.accentSoft,
                lightsOut.signal, lightsOut.warning, lightsOut.danger,
                lightsOut.muted, lightsOut.onAccent, lightsOut.sidebarScrim,
            ],
            [
                dark.recessed, dark.panel, dark.raised, dark.line,
                dark.accent, dark.accentFill, dark.accentSoft,
                dark.signal, dark.warning, dark.danger,
                dark.muted, dark.onAccent, dark.sidebarScrim,
            ]
        )
    }

    func testGitBranchSwitchUsesAnAdvertisedBranch() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.gitStatus = GitStatus(currentBranch: "main", branches: ["feature", "main"])

        model.switchGitBranch(to: "unknown")
        let requestCount = await recorder.requestCount()
        model.switchGitBranch(to: "feature")
        let request = await recorder.firstRequest(after: requestCount) { request in
            if case .switchGitBranch = request { return true }
            return false
        }

        let requests = await recorder.requests()
        XCTAssertEqual(requests.count, 1)
        guard case .switchGitBranch(_, let sessionID, let branch) = try XCTUnwrap(request) else {
            return XCTFail("Expected a branch switch request")
        }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(branch, "feature")
    }

}
