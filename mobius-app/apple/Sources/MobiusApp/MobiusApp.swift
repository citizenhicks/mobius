import SwiftUI

@main
struct MobiusAppleApp: App {
    @UIApplicationDelegateAdaptor(MobiusAppDelegate.self) private var appDelegate
    @State private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            AppShell()
                .mobiusTheme()
                .environment(model)
                .onAppear { appDelegate.attach(model) }
        }
    }
}
