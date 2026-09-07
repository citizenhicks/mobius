import Foundation
@testable import Mobius
import XCTest

extension GatewayWireTests {
    func testProtocolV28PairAndAuthenticateRequireClientKind() throws {
        let pair = try requestObject(.pair(
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

    func testProtocolV28ClientInventoryRoundTrip() throws {
        let request = try requestObject(.listClients(requestID: "clients-1"))
        XCTAssertEqual(request["type"] as? String, "list_clients")
        XCTAssertEqual(request["request_id"] as? String, "clients-1")
        let unpair = try requestObject(.unpairClient(
            requestID: "unpair-1",
            clientID: "phone-7"
        ))
        XCTAssertEqual(unpair["type"] as? String, "unpair_client")
        XCTAssertEqual(unpair["client_id"] as? String, "phone-7")

        let fixture = #"{"version":28,"type":"clients","request_id":"clients-1","current_client_id":"mac-2","clients":[{"client_id":"phone-7","label":"Phone","kinds":["ios"],"connections":1},{"client_id":"mac-2","label":"Mac","kinds":[],"connections":0}]}"#
        guard case .clients(let requestID, let currentClientID, let clients) = try decodeEnvelope(fixture) else {
            return XCTFail("Expected client inventory envelope")
        }
        XCTAssertEqual(requestID, "clients-1")
        XCTAssertEqual(currentClientID, "mac-2")
        XCTAssertEqual(clients.first?.clientId, "phone-7")
        XCTAssertEqual(clients.first?.kinds, [.ios])
        XCTAssertEqual(clients.last?.connections, 0)
    }

}
