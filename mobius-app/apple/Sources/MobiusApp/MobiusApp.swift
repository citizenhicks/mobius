import SwiftUI

@main
struct MobiusAppleApp: App {
    @State private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            AppShell()
                .mobiusTheme()
                .environment(model)
                .onOpenURL { model.handleOpenURL($0) }
        }
    }
}
