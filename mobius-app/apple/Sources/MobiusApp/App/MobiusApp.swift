import AppIntents
import SwiftUI

@main
struct MobiusAppleApp: App {
    @UIApplicationDelegateAdaptor(MobiusAppDelegate.self) private var appDelegate
    private let sceneRegistry: AppSceneRegistry
    private let cloudPurchases: MobiusCloudPurchases

    init() {
        let sceneRegistry = AppSceneRegistry()
        self.sceneRegistry = sceneRegistry
        cloudPurchases = .live()
        AppDependencyManager.shared.add(dependency: sceneRegistry)
    }

    var body: some Scene {
        WindowGroup(for: AppWindowState.self) { $windowState in
            MobiusWindow(
                state: $windowState,
                appDelegate: appDelegate,
                sceneRegistry: sceneRegistry,
                cloudPurchases: cloudPurchases
            )
        } defaultValue: {
            AppWindowState()
        }
    }
}

private struct MobiusWindow: View {
    @Binding var state: AppWindowState
    let appDelegate: MobiusAppDelegate
    let sceneRegistry: AppSceneRegistry
    let cloudPurchases: MobiusCloudPurchases

    @Environment(\.scenePhase) private var scenePhase
    @State private var model: AppModel

    init(
        state: Binding<AppWindowState>,
        appDelegate: MobiusAppDelegate,
        sceneRegistry: AppSceneRegistry,
        cloudPurchases: MobiusCloudPurchases
    ) {
        _state = state
        self.appDelegate = appDelegate
        self.sceneRegistry = sceneRegistry
        self.cloudPurchases = cloudPurchases
        _model = State(
            initialValue: AppModel(
                cloudPurchases: cloudPurchases,
                windowState: state.wrappedValue
            ))
    }

    var body: some View {
        AppShell()
            .mobiusTheme()
            .environment(model)
            .onAppear { sceneRegistry.register(model) }
            .onDisappear {
                model.appDidEnterBackground(preservingVoiceCall: false)
                sceneRegistry.unregister(model)
                appDelegate.attach(sceneRegistry.activeModel?.cloud)
            }
            .onChange(of: scenePhase, initial: true) { _, phase in
                sceneRegistry.setActive(phase == .active, model: model)
                appDelegate.attach(sceneRegistry.activeModel?.cloud)
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
                sceneRegistry.cloudAuthenticationDidChange(in: model)
            }
            .onChange(of: model.destination, initial: true) { _, destination in
                if let destination { state.destination = destination }
            }
            .onChange(of: model.navigationPath, initial: true) { _, navigationPath in
                state.navigationPath = navigationPath
            }
    }
}

@MainActor
final class AppSceneRegistry {
    private final class WeakModel {
        weak var model: AppModel?

        init(_ model: AppModel) {
            self.model = model
        }
    }

    private var activeModels: [ObjectIdentifier: WeakModel] = [:]
    private var registeredModels: [ObjectIdentifier: WeakModel] = [:]
    private weak var latestModel: AppModel?
    private weak var latestActiveModel: AppModel?
    private weak var cloudPurchaseModel: AppModel?
    private var opensChatsWhenSceneRegisters = false

    var activeModel: AppModel? {
        if let latestActiveModel,
            activeModels[ObjectIdentifier(latestActiveModel)]?.model != nil
        {
            return latestActiveModel
        }
        return activeModels.values.lazy.compactMap(\.model).first
    }

    func register(_ model: AppModel) {
        registeredModels[ObjectIdentifier(model)] = WeakModel(model)
        latestModel = model
        updateCloudPurchaseObserver()
        guard opensChatsWhenSceneRegisters else { return }
        opensChatsWhenSceneRegisters = false
        openChats(in: model)
    }

    func unregister(_ model: AppModel) {
        let id = ObjectIdentifier(model)
        activeModels.removeValue(forKey: id)
        registeredModels.removeValue(forKey: id)
        if latestModel === model {
            latestModel = registeredModels.values.lazy.compactMap(\.model).first
        }
        if latestActiveModel === model { latestActiveModel = activeModel }
        updateCloudPurchaseObserver()
    }

    func setActive(_ active: Bool, model: AppModel) {
        register(model)
        let id = ObjectIdentifier(model)
        if active {
            activeModels[id] = WeakModel(model)
            latestActiveModel = model
        } else {
            activeModels.removeValue(forKey: id)
            if latestActiveModel === model { latestActiveModel = activeModel }
        }
    }

    func openChats() {
        guard let model = activeModel ?? latestModel else {
            opensChatsWhenSceneRegisters = true
            return
        }
        openChats(in: model)
    }

    func cloudAuthenticationDidChange(in model: AppModel) {
        model.cloud.scheduleAuthenticationRefresh()
        guard model.cloud.cloudSession == nil else {
            setCloudPurchaseObserver(model)
            return
        }
        for other in registeredModels.values.lazy.compactMap(\.model)
        where other !== model && other.cloud.cloudSession != nil {
            Task { await other.cloud.applyCloudSignOutFromAnotherWindow() }
        }
    }

    private func openChats(in model: AppModel) {
        model.destination = .chats
        model.navigationPath = []
    }

    private func updateCloudPurchaseObserver() {
        if let cloudPurchaseModel,
            registeredModels[ObjectIdentifier(cloudPurchaseModel)]?.model != nil
        {
            return
        }
        cloudPurchaseModel?.cloud.stopObservingCloudPurchaseUpdates()
        if let model = activeModel ?? latestModel { setCloudPurchaseObserver(model) }
    }

    private func setCloudPurchaseObserver(_ model: AppModel) {
        guard cloudPurchaseModel !== model else { return }
        cloudPurchaseModel?.cloud.stopObservingCloudPurchaseUpdates()
        cloudPurchaseModel = model
        model.cloud.observeCloudPurchaseUpdates()
    }
}

struct OpenChatsIntent: AppIntent {
    static let title: LocalizedStringResource = "Open Chats"
    static let description = IntentDescription("Opens möbius to the chat list.")
    static let supportedModes: IntentModes = .foreground(.immediate)

    @Dependency private var sceneRegistry: AppSceneRegistry

    @MainActor
    func perform() async throws -> some IntentResult {
        sceneRegistry.openChats()
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
