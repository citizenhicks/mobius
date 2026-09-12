import AuthenticationServices
import Foundation
import SwiftUI
@testable import Mobius
import XCTest

@MainActor
final class WelcomeTests: XCTestCase {
    func testCurrentPlanNamesFollowVerifiedAccessAndSettingsRenderAcrossLifecycle() async throws {
        let userID = UUID()
        let suite = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 402, height: 874)
        window.overrideUserInterfaceStyle = .light
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        let gateway = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://preview.sprites.app"),
            displayName: "möbius Cloud",
            machineName: "möbius Cloud",
            cloudUserID: userID
        )
        let selfHosted = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://gateway.example"),
            displayName: "My computer",
            machineName: "My Mac"
        )
        let cases: [(String, MobiusCloudTier, String, Bool, String?)] = [
            ("Cloud", .cloud, "active", true, nil),
            ("Cloud Plus", .cloudPlus, "active", true, nil),
            ("Pending downgrade", .cloudPlus, "active", true, "cloud"),
            ("Cancelled renewal", .cloudPlus, "active", false, nil),
            ("Grace period", .cloudPlus, "grace_period", true, "cloud"),
            ("Billing retry", .cloudPlus, "billing_retry", true, nil),
            ("Expired", .cloudPlus, "expired", false, nil),
            ("Refunded", .cloudPlus, "revoked", false, nil),
            ("Paid downgrade", .cloud, "active", true, nil),
        ]
        for (name, tier, status, renews, next) in cases {
            let subscribed = status == "active" || status == "grace_period"
            let sessionStore = MobiusCloudSessionStore(service: "subscription-preview.\(UUID())")
            defer { try? sessionStore.remove() }
            let payload: [String: Any] = [
                "userId": userID.uuidString, "email": "you@example.com",
                "subscribed": subscribed, "sharesDiagnostics": false,
                "subscriptionStartedAt": subscribed ? "2026-09-08T08:00:00Z" as Any : NSNull(),
                "subscription": [
                    "tier": tier.rawValue, "productId": tier.productID, "status": status,
                    "billingPeriodStartedAt": "2026-09-08T08:00:00Z",
                    "billingPeriodEndsAt": "2026-10-08T08:00:00Z",
                    "accessEndsAt": "2026-10-08T08:00:00Z", "autoRenews": renews,
                    "nextTier": next as Any? ?? NSNull(),
                ],
                "luna": subscribed
                    ? [
                        "creditMicrousd": 100, "remainingMicrousd": 72,
                        "resetsAt": renews ? "2026-10-08T08:00:00Z" as Any : NSNull(),
                    ] as Any : NSNull(),
            ]
            var accountPayload = payload
            if status != "active" {
                var subscription = try XCTUnwrap(accountPayload["subscription"] as? [String: Any])
                subscription["billingPeriodStartedAt"] = "2026-08-07T08:00:00Z"
                subscription["billingPeriodEndsAt"] = "2026-09-07T08:00:00Z"
                subscription["accessEndsAt"] =
                    status == "grace_period"
                    ? "2026-09-10T08:00:00Z" : "2026-09-07T08:00:00Z"
                accountPayload["subscription"] = subscription
                if subscribed {
                    accountPayload["subscriptionStartedAt"] = "2026-08-07T08:00:00Z"
                    accountPayload["luna"] = [
                        "creditMicrousd": 100, "remainingMicrousd": 72, "resetsAt": NSNull(),
                    ]
                }
            }
            let data = try JSONSerialization.data(withJSONObject: accountPayload)
            let client = MobiusCloudClient(store: sessionStore) { request in
                let response = try XCTUnwrap(
                    HTTPURLResponse(
                        url: XCTUnwrap(request.url), statusCode: 200, httpVersion: nil,
                        headerFields: ["Content-Type": "application/json"]
                    ))
                if request.url?.path == "/api/mobile/auth/apple" {
                    return (
                        Data(
                            """
                            {"token":"\(String(repeating: "t", count: 43))","userId":"\(userID)","expiresAt":"2099-01-01T00:00:00Z"}
                            """.utf8), response
                    )
                }
                return (data, response)
            }
            _ = try await client.authenticate(
                authorizationCode: "preview", nonce: String(repeating: "n", count: 43))
            let model = AppModel(
                store: GatewayStore(defaults: defaults), settingsDefaults: defaults,
                cloudClient: client,
                cloudPurchases: MobiusCloudPurchases(
                    displayPrices: { [:] },
                    unfinishedPurchases: { MobiusCloudPurchaseScan() },
                    currentEntitlements: { _ in MobiusCloudPurchaseScan() },
                    purchase: { _, _ in throw MobiusCloudPurchaseError.unavailable }
                )
            )
            model.gateway.accounts = [gateway, selfHosted]
            model.gateway.selectedAccountID = gateway.id
            await model.cloud.refreshCloudAccount()
            XCTAssertEqual(model.cloud.currentTier, subscribed ? tier : nil, name)
            XCTAssertEqual(
                model.cloud.gatewayName(gateway), "möbius \(subscribed ? tier.name : "Cloud")", name
            )
            XCTAssertEqual(model.cloud.gatewayName(selfHosted), "My Mac", name)
            var pages: [(String, AnyView)] = [
                ("Settings", AnyView(NavigationStack { ProfileView(expandedSection: .account) })),
                ("Sidebar", AnyView(SidebarView { _ in }.frame(width: SidebarDrawerMetrics.width))),
            ]
            if name == "Cloud Plus" {
                for section: ProfileView.SettingsSection? in [nil, .appearance, .data, .usage] {
                    pages.append(
                        (
                            section?.rawValue ?? "Overview",
                            AnyView(NavigationStack { ProfileView(expandedSection: section) })
                        ))
                }
            }
            for (page, content) in pages {
                let host = UIHostingController(
                    rootView:
                        content
                        .environment(model)
                        .environment(\.mobiusPalette, MobiusPalette(.light))
                        .environment(\.locale, Locale(identifier: "en_US"))
                        .environment(\.scenePhase, .inactive)
                        .preferredColorScheme(.light)
                        .transaction { $0.disablesAnimations = true })
                window.rootViewController = host
                window.makeKeyAndVisible()
                try await Task.sleep(for: .milliseconds(150))
                host.view.layoutIfNeeded()
                if page != "Sidebar" {
                    try assertSettingsSections(in: host.view, collapsed: page == "Overview")
                }
                let attachment = XCTAttachment(
                    image: UIGraphicsImageRenderer(bounds: host.view.bounds).image { _ in
                        host.view.drawHierarchy(in: host.view.bounds, afterScreenUpdates: true)
                    })
                attachment.name = "\(page) - \(name)"
                attachment.lifetime = .keepAlways
                add(attachment)
            }
            model.cloud.cloudAccount = nil
            XCTAssertNil(model.cloud.currentTier)
            XCTAssertEqual(model.cloud.gatewayName(gateway), "möbius Cloud")
        }
    }

    private func assertSettingsSections(in view: UIView, collapsed: Bool) throws {
        func collection(in view: UIView) -> UICollectionView? {
            if let collection = view as? UICollectionView { return collection }
            return view.subviews.lazy.compactMap { collection(in: $0) }.first
        }
        let list = try XCTUnwrap(collection(in: view))
        let rowCounts = (0..<list.numberOfSections).map { list.numberOfItems(inSection: $0) }
        XCTAssertEqual(rowCounts.count, 4)
        XCTAssertEqual(rowCounts.filter { $0 > 1 }.count, collapsed ? 0 : 1)
        if collapsed {
            XCTAssertEqual(rowCounts, [1, 1, 1, 1])
            XCTAssertLessThan(list.contentSize.height, view.bounds.height)
        }
    }

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
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudPurchases: MobiusCloudPurchases(
                displayPrices: { [.cloud: "$5.99", .cloudPlus: "$15.99"] },
                unfinishedPurchases: { MobiusCloudPurchaseScan() },
                currentEntitlements: { _ in MobiusCloudPurchaseScan() },
                purchase: { _, _ in throw MobiusCloudPurchaseError.unavailable }
            )
        )
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 402, height: 874)
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
                rootView: ZStack {
                    MobiusBackdrop()
                    PairingView(canCancel: true, initialSetup: .cloud)
                        .padding(MobiusSpace.xl)
                }
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
            func descendants<T: UIView>(of type: T.Type, in view: UIView) -> [T] {
                (view as? T).map { [$0] } ?? view.subviews.flatMap { descendants(of: type, in: $0) }
            }
            let picker = try XCTUnwrap(
                descendants(of: UISegmentedControl.self, in: host.view).first)
            XCTAssertEqual(picker.numberOfSegments, 2)
            XCTAssertEqual(picker.titleForSegment(at: 0), "Cloud")
            XCTAssertEqual(picker.titleForSegment(at: 1), "Cloud Plus")
            for tier in 0..<2 {
                picker.selectedSegmentIndex = tier
                picker.sendActions(for: .valueChanged)
                try await Task.sleep(for: .milliseconds(150))
                host.view.layoutIfNeeded()
                // Capture the controls after scrolling, as on a compact phone.
                if let scroll = descendants(of: UIScrollView.self, in: host.view).first {
                    scroll.setContentOffset(
                        CGPoint(x: 0, y: max(0, scroll.contentSize.height - scroll.bounds.height)),
                        animated: false
                    )
                }
                let attachment = XCTAttachment(
                    image: UIGraphicsImageRenderer(bounds: host.view.bounds).image { _ in
                        host.view.drawHierarchy(in: host.view.bounds, afterScreenUpdates: true)
                    })
                attachment.name =
                    "Cloud offer \(tier == 0 ? "Cloud" : "Plus") \(dark ? "dark" : "light")"
                attachment.lifetime = .keepAlways
                add(attachment)
            }
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
            (
                "New routine",
                AnyView(
                    RoutineForm(
                        botID: "bot-1",
                        workspaces: [RoutineWorkspace(path: "/srv/mobius", name: "möbius")]
                    ) {})
            ),
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
