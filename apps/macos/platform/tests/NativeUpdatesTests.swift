import Foundation
import Sparkle
import Testing

@testable import MobiusNative

private let key = Data(repeating: 7, count: 32).base64EncodedString()

@Test(arguments: ["0.3.13", "0.16.11"])
func independentBuildSupersedesEarlierDesktopVersions(previous: String) {
    #expect(SUStandardVersionComparator.default.compareVersion("1", toVersion: previous) == .orderedDescending)
}

@Test func onlyAConfiguredAppBundleCanUpdate() {
    let app = URL(fileURLWithPath: "/Applications/möbius.app")
    let info: [String: Any] = [
        "CFBundleIdentifier": "app.mobius.desktop",
        "SUFeedURL": "https://example.com/appcast.xml", "SUPublicEDKey": key,
    ]
    #expect(UpdateAvailability.reason(bundleURL: app, info: info) == nil)
    let unbundled = URL(fileURLWithPath: "/tmp/target/debug")
    #expect(
        UpdateAvailability.reason(bundleURL: unbundled, info: info)
            == UpdateAvailability.development)
    var development = info
    development["SUFeedURL"] = nil
    #expect(
        UpdateAvailability.reason(bundleURL: app, info: development)
            == UpdateAvailability.development)
    var placeholder = info
    placeholder["SUPublicEDKey"] = "REPLACE_WITH_SPARKLE_PUBLIC_ED_KEY"
    #expect(
        UpdateAvailability.reason(bundleURL: app, info: placeholder)
            == UpdateAvailability.unconfigured)
}
