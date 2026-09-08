import Foundation
@testable import Mobius
import XCTest

@MainActor
final class WelcomeTests: XCTestCase {
    func testWelcomeCompletionSurvivesRelaunchAndExistingGatewaysSkipIt() async throws {
        let suite = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = GatewayStore(defaults: defaults)
        let firstLaunch = AppModel(store: store, settingsDefaults: defaults)
        XCTAssertTrue(firstLaunch.showsWelcome)
        firstLaunch.completeWelcome()
        XCTAssertFalse(firstLaunch.showsWelcome)
        XCTAssertFalse(AppModel(store: store, settingsDefaults: defaults).showsWelcome)

        defaults.removeObject(forKey: "welcome-completed")
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try store.save(account, token: "welcome-test")
        addTeardownBlock { try await store.remove(account) }
        XCTAssertFalse(AppModel(store: store, settingsDefaults: defaults).showsWelcome)
        XCTAssertTrue(defaults.bool(forKey: "welcome-completed"))
    }

    func testManualLinksUseSupportedLanguageAndPreserveTheSection() {
        for (locale, language) in [
            ("en-US", "en"), ("fr-CA", "fr"), ("de-DE", "de"), ("ja-JP", "en"),
        ] {
            for section in ["gateway", "bots", "bot-defaults", "providers", "extensions", "start"] {
                XCTAssertEqual(
                    userManualURL(section: section, locale: Locale(identifier: locale))
                        .absoluteString,
                    "https://mobius.thinkingsand.dev/how?lang=\(language)#\(section)"
                )
            }
        }
    }
}
