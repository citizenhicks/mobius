import Foundation
import SwiftUI
import UIKit
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

    func testAppearanceUsesTheInjectedDefaults() throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        defaults.set(ThemePreference.lightsOut.rawValue, forKey: "theme")
        defaults.set(AccentTint.purple.rawValue, forKey: "accent-tint")
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults
        )

        XCTAssertEqual(model.theme, .lightsOut)
        XCTAssertEqual(model.accentTint, .purple)
        model.setTheme(.light)
        model.setAccentTint(.orange)
        XCTAssertEqual(defaults.string(forKey: "theme"), ThemePreference.light.rawValue)
        XCTAssertEqual(defaults.string(forKey: "accent-tint"), AccentTint.orange.rawValue)
    }

    func testLightsOutUsesBlackCanvasWithDarkPalette() {
        let dark = MobiusPalette(.dark)
        let lightsOut = MobiusPalette(.light, lightsOut: true)

        XCTAssertEqual(lightsOut.canvas, .black)
        XCTAssertEqual(lightsOut.recessed, .black)
        XCTAssertEqual(
            [
                lightsOut.panel, lightsOut.raised, lightsOut.line,
                lightsOut.accent, lightsOut.accentFill, lightsOut.accentSoft,
                lightsOut.signal, lightsOut.warning, lightsOut.danger,
                lightsOut.muted, lightsOut.onAccent, lightsOut.onDanger,
                lightsOut.onMedia, lightsOut.shadow, lightsOut.sidebarScrim,
            ],
            [
                dark.panel, dark.raised, dark.line,
                dark.accent, dark.accentFill, dark.accentSoft,
                dark.signal, dark.warning, dark.danger,
                dark.muted, dark.onAccent, dark.onDanger,
                dark.onMedia, dark.shadow, dark.sidebarScrim,
            ]
        )
    }

    func testAccentTintColorsThePalette() {
        let tint = AccentTint.purple
        let dark = MobiusPalette(.dark, accentTint: tint)
        let light = MobiusPalette(.light, accentTint: tint)
        let defaultDark = MobiusPalette(.dark)
        let defaultLight = MobiusPalette(.light)

        XCTAssertEqual(
            dark.accent,
            tint.color.mix(with: .white, by: 0.55, in: .device)
        )
        XCTAssertEqual(
            dark.accentFill,
            tint.color.mix(with: .black, by: 0.5, in: .device)
        )
        XCTAssertEqual(
            dark.accentSoft,
            dark.panel.mix(with: tint.color, by: 0.08, in: .device)
        )
        XCTAssertEqual(
            light.accent,
            tint.color.mix(with: .black, by: 0.55, in: .device)
        )
        XCTAssertEqual(
            light.accentFill,
            tint.color.mix(with: .black, by: 0.6, in: .device)
        )
        XCTAssertEqual(
            light.accentSoft,
            light.panel.mix(with: tint.color, by: 0.12, in: .device)
        )
        XCTAssertTrue(zip(
            [dark.canvas, dark.recessed, dark.panel, dark.raised, dark.line, dark.sidebarScrim],
            [
                defaultDark.canvas, defaultDark.recessed, defaultDark.panel,
                defaultDark.raised, defaultDark.line, defaultDark.sidebarScrim,
            ]
        ).allSatisfy { $0.0 != $0.1 })
        XCTAssertTrue(zip(
            [light.canvas, light.recessed, light.panel, light.raised, light.line, light.sidebarScrim],
            [
                defaultLight.canvas, defaultLight.recessed, defaultLight.panel,
                defaultLight.raised, defaultLight.line, defaultLight.sidebarScrim,
            ]
        ).allSatisfy { $0.0 != $0.1 })
    }

    func testAccentTintsMeetTextContrastInEveryAppearance() {
        let appearances: [(ColorScheme, Bool)] = [(.light, false), (.dark, false), (.dark, true)]
        for (scheme, lightsOut) in appearances {
            for tint in AccentTint.allCases {
                let palette = MobiusPalette(scheme, lightsOut: lightsOut, accentTint: tint)
                for surface in [
                    palette.canvas, palette.recessed, palette.panel,
                    palette.raised, palette.accentSoft,
                ] {
                    XCTAssertGreaterThanOrEqual(
                        palette.accent.contrastRatio(with: surface),
                        4.5,
                        "\(tint.label) in \(scheme) mode"
                    )
                    XCTAssertGreaterThanOrEqual(
                        palette.muted.contrastRatio(with: surface),
                        3,
                        "\(tint.label) muted text in \(scheme) mode"
                    )
                }
                XCTAssertGreaterThanOrEqual(
                    palette.onAccent.contrastRatio(with: palette.accentFill),
                    4.5,
                    "\(tint.label) fill in \(scheme) mode"
                )
            }
        }
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

@MainActor
private extension Color {
    func contrastRatio(with other: Color) -> CGFloat {
        let brighter = max(relativeLuminance, other.relativeLuminance)
        let darker = min(relativeLuminance, other.relativeLuminance)
        return (brighter + 0.05) / (darker + 0.05)
    }

    var relativeLuminance: CGFloat {
        var red: CGFloat = 0
        var green: CGFloat = 0
        var blue: CGFloat = 0
        var alpha: CGFloat = 0
        guard UIColor(self).getRed(&red, green: &green, blue: &blue, alpha: &alpha) else {
            return 0
        }
        let channels = [red, green, blue].map { component in
            component <= 0.04045
                ? component / 12.92
                : pow((component + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2]
    }
}
