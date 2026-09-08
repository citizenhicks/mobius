import AuthenticationServices
import Foundation
import SwiftUI
@testable import Mobius
import XCTest

@MainActor
final class WelcomeTests: XCTestCase {
    func testPairingStartsWithChoicesAndPastedSetupRevealsSecureFields() async throws {
        let suite = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let model = AppModel(store: GatewayStore(defaults: defaults), settingsDefaults: defaults)
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 390, height: 780)
        window.overrideUserInterfaceStyle = .light
        let host = UIHostingController(
            rootView: PairingView(canCancel: true)
                .environment(model)
                .environment(\.mobiusPalette, MobiusPalette(.light))
                .environment(\.scenePhase, .inactive)
                .transaction { $0.disablesAnimations = true }
                .padding(24)
        )
        window.rootViewController = host
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        func fields(in view: UIView) -> [UITextField] {
            (view as? UITextField).map { [$0] } ?? view.subviews.flatMap { fields(in: $0) }
        }
        host.view.layoutIfNeeded()
        XCTAssertTrue(fields(in: host.view).isEmpty)
        model.applyPairingSetup(
            try GatewayPairingSetup(endpoint: "wss://gateway.example", code: "TEST-CODE"))
        for _ in 0..<100 where fields(in: host.view).count < 2 {
            try await Task.sleep(for: .milliseconds(10))
            host.view.layoutIfNeeded()
        }
        let inputs = fields(in: host.view)
        XCTAssertEqual(inputs.count, 2)
        XCTAssertTrue(inputs.contains { $0.text == "wss://gateway.example" })
        let code = try XCTUnwrap(inputs.first { $0.isSecureTextEntry })
        XCTAssertEqual(code.text, "TEST-CODE")
        func scrollView(in view: UIView) -> UIScrollView? {
            if let scroll = view as? UIScrollView { return scroll }
            return view.subviews.lazy.compactMap { scrollView(in: $0) }.first
        }
        let scroll = try XCTUnwrap(scrollView(in: host.view))
        let formSize = scroll.contentSize
        model.gateway.pairingError =
            "The one-time code has expired. Request a new code from your gateway and try again."
        try await Task.sleep(for: .milliseconds(50))
        host.view.layoutIfNeeded()
        XCTAssertEqual(
            scroll.contentSize, formSize, "Header messages must not push the form controls")
        let attachment = XCTAttachment(
            image: UIGraphicsImageRenderer(bounds: host.view.bounds).image { _ in
                host.view.drawHierarchy(in: host.view.bounds, afterScreenUpdates: true)
            })
        attachment.name = "Gateway pairing with status above the form"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testCloudOfferKeepsNativeAppleAuthorizationInBothAppearances() async throws {
        let suite = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let model = AppModel(store: GatewayStore(defaults: defaults), settingsDefaults: defaults)
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 390, height: 844)
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        func hasAppleButton(in view: UIView) -> Bool {
            view is ASAuthorizationAppleIDButton
                || view.subviews.contains { hasAppleButton(in: $0) }
        }
        for dark in [false, true] {
            window.overrideUserInterfaceStyle = dark ? .dark : .light
            let host = UIHostingController(
                rootView: PairingView(canCancel: true, initialSetup: .cloud)
                    .environment(model)
                    .environment(\.mobiusPalette, MobiusPalette(dark ? .dark : .light))
                    .environment(\.scenePhase, .inactive)
                    .preferredColorScheme(dark ? .dark : .light)
                    .transaction { $0.disablesAnimations = true }
            )
            window.rootViewController = host
            window.makeKeyAndVisible()
            for _ in 0..<100 where !hasAppleButton(in: host.view) {
                try await Task.sleep(for: .milliseconds(10))
                host.view.layoutIfNeeded()
            }
            XCTAssertTrue(hasAppleButton(in: host.view))
            let attachment = XCTAttachment(
                image: UIGraphicsImageRenderer(bounds: host.view.bounds).image { _ in
                    host.view.drawHierarchy(in: host.view.bounds, afterScreenUpdates: true)
                })
            attachment.name = dark ? "Cloud offer dark" : "Cloud offer light"
            attachment.lifetime = .keepAlways
            add(attachment)
        }
    }

    func testInlineSetupFormsRenderAtCompactWidth() async throws {
        let suite = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let model = AppModel(store: GatewayStore(defaults: defaults), settingsDefaults: defaults)
        model.workspace = WorkspaceInfo(id: "workspace", path: "/srv/projects/mobius")
        model.chat.selectedModelRoute = "openai/gpt-5.6-sol"
        model.chat.attachedFolders = ["/srv/design/assets", "/srv/shared/product-research"]
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 350, height: 780)
        window.overrideUserInterfaceStyle = .light
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        for (name, content) in [
            ("New Bot", AnyView(NewBotForm {})),
            ("New Swarm", AnyView(NewSwarmForm {})),
            ("Install extension", AnyView(InstallExtensionForm {})),
            ("Chat info", AnyView(ChatInfoView())),
        ] {
            let host = UIHostingController(
                rootView:
                    content
                    .padding(24)
                    .environment(model)
                    .environment(\.mobiusPalette, MobiusPalette(.light))
                    .preferredColorScheme(.light))
            window.rootViewController = host
            window.makeKeyAndVisible()
            try await Task.sleep(for: .milliseconds(50))
            host.view.layoutIfNeeded()
            XCTAssertGreaterThan(host.view.intrinsicContentSize.height, 100)
            let attachment = XCTAttachment(
                image: UIGraphicsImageRenderer(bounds: host.view.bounds).image { _ in
                    host.view.drawHierarchy(in: host.view.bounds, afterScreenUpdates: true)
                })
            attachment.name = name
            attachment.lifetime = .keepAlways
            add(attachment)
        }
    }

    func testSetupIllustrationsIncludeTheVendoredArtworkAtCompactWidth() throws {
        XCTAssertNotNil(UIImage(named: MobiusGlyph.smartPhone01.asset))
        for (name, scene) in [
            ("Gateway", SetupArtwork.Scene.gateway), ("Bot", .bot), ("Workspace", .workspace),
        ] {
            let renderer = ImageRenderer(
                content: SetupArtwork(scene: scene, active: false)
                    .frame(width: 280, height: 220)
                    .environment(\.mobiusPalette, MobiusPalette(.light)))
            let image = try XCTUnwrap(renderer.uiImage)
            XCTAssertEqual(image.size, CGSize(width: 280, height: 220))
            let attachment = XCTAttachment(image: image)
            attachment.name = name
            attachment.lifetime = .keepAlways
            add(attachment)
        }
    }

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
