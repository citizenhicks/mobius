import AppIntents
import SwiftUI

@main
struct MobiusAppleApp: App {
    @UIApplicationDelegateAdaptor(MobiusAppDelegate.self) private var appDelegate
    @State private var model: AppModel
    @Environment(\.scenePhase) private var scenePhase

    init() {
        let model = AppModel()
        _model = State(initialValue: model)
        AppDependencyManager.shared.add(dependency: model)
    }

    var body: some Scene {
        WindowGroup {
            AppShell()
                .mobiusTheme()
                .environment(model)
        }
        .onChange(of: scenePhase, initial: true) { _, phase in
            appDelegate.attach(model.cloud)
            switch phase {
            case .background:
                model.appDidEnterBackground()
            case .active:
                model.beginAppActivation()
            case .inactive:
                break
            @unknown default:
                break
            }
        }
        .onChange(of: model.cloud.cloudSession?.credentialID) { _, _ in
            model.cloud.scheduleAuthenticationRefresh()
        }
    }
}

struct OpenChatsIntent: AppIntent {
    static let title: LocalizedStringResource = "Open Chats"
    static let description = IntentDescription("Opens möbius to the chat list.")
    static let supportedModes: IntentModes = .foreground(.immediate)

    @Dependency private var model: AppModel

    @MainActor
    func perform() async throws -> some IntentResult {
        model.destination = .chats
        model.navigationPath = []
        return .result()
    }
}

struct MobiusShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: OpenChatsIntent(),
            phrases: [
                "Show my chats in \(.applicationName)",
                "Open chats in \(.applicationName)",
            ],
            shortTitle: "Open Chats",
            systemImageName: "bubble.left.and.bubble.right"
        )
    }
}
