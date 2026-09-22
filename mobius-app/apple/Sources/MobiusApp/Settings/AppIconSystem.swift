import UIKit

@MainActor
struct AppIconSystem {
    let supportsAlternateIcons: () -> Bool
    let alternateIconName: () -> String?
    let setAlternateIconName: (String?) async throws -> Void

    static func live() -> Self {
        Self(
            supportsAlternateIcons: { UIApplication.shared.supportsAlternateIcons },
            alternateIconName: { UIApplication.shared.alternateIconName },
            setAlternateIconName: { try await UIApplication.shared.setAlternateIconName($0) }
        )
    }
}
