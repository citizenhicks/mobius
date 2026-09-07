import SwiftUI

@main
struct MobiusAppleApp: App {
    @UIApplicationDelegateAdaptor(MobiusAppDelegate.self) private var appDelegate
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

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
