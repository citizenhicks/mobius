import Foundation
@testable import Mobius
import XCTest

extension GatewayWireTests {
    func testProtocolV28PairAndAuthenticateRequireClientKind() throws {
        let pair = try requestObject(
            .pair(
                code: "123456",
                clientLabel: "Phone",
                clientKind: .ios
            ))
        XCTAssertEqual(pair["type"] as? String, "pair")
        XCTAssertEqual(pair["client_label"] as? String, "Phone")
        XCTAssertEqual(pair["client_kind"] as? String, "ios")
        XCTAssertNil(pair["last_sequence"])

        let authenticate = try requestObject(.authenticate(token: "bearer", clientKind: .macos))
        XCTAssertEqual(authenticate["type"] as? String, "authenticate")
        XCTAssertEqual(authenticate["token"] as? String, "bearer")
        XCTAssertEqual(authenticate["client_kind"] as? String, "macos")
        XCTAssertNil(authenticate["last_sequence"])
    }

}
