import Foundation
import XCTest

extension GatewayWireTests {
    func testUnsupportedGatewayVersionIsRejected() {
        let fixture = #"{"version":20,"type":"authenticated"}"#
        XCTAssertThrowsError(
            try decoder().decode(GatewayEnvelope.self, from: Data(fixture.utf8))
        ) { error in
            XCTAssertEqual(error as? GatewayWireError, .unsupportedVersion(20))
        }
    }

    func testPlaintextEndpointIsRestrictedToLoopback() throws {
        XCTAssertEqual(try GatewayEndpoint("tcp://localhost:9191").rawValue, "tcp://localhost:9191")
        XCTAssertThrowsError(try GatewayEndpoint("tcp://example.com:9191")) { error in
            XCTAssertEqual(error as? GatewayWireError, .insecureRemoteEndpoint)
        }
        XCTAssertEqual(try GatewayEndpoint("tls://example.com:443").rawValue, "tls://example.com:443")
    }

    func testSecureWebSocketEndpointUsesImplicitPort443() throws {
        let endpoint = try GatewayEndpoint("wss://gateway.example/")

        XCTAssertEqual(endpoint.rawValue, "wss://gateway.example")
        XCTAssertEqual(endpoint.host, "gateway.example")
        XCTAssertEqual(endpoint.port, 443)
        XCTAssertTrue(endpoint.usesTLS)
        XCTAssertTrue(endpoint.usesWebSocket)
        XCTAssertEqual(
            try GatewayEndpoint("wss://gateway.example:8443").rawValue,
            "wss://gateway.example:8443"
        )
        XCTAssertThrowsError(try GatewayEndpoint("ws://gateway.example"))
        XCTAssertThrowsError(try GatewayEndpoint("wss://gateway.example/chat"))
    }

    func testGatewayAccountsGetFriendlyDefaultNames() throws {
        let quick = GatewayAccount(endpoint: try GatewayEndpoint(
            "wss://pupils-convention-ban-format.trycloudflare.com"
        ))
        let named = GatewayAccount(endpoint: try GatewayEndpoint("wss://gateway.example"))

        XCTAssertEqual(quick.displayName, "Cloudflare · pupils…format")
        XCTAssertEqual(named.displayName, "gateway.example")
    }

    func testPairingSetupParsesValidatedEndpointAndCode() throws {
        let setup = try GatewayPairingSetup(
            "mobius-pair:v1|wss://gateway.example:443|0123456789abcdef"
        )

        XCTAssertEqual(setup.endpoint.rawValue, "wss://gateway.example")
        XCTAssertEqual(setup.code, "0123456789abcdef")
    }

    func testPairingSetupRejectsMalformedOrUnsafeValues() {
        let invalid = [
            "mobius-pair:v2|wss://gateway.example|code",
            "mobius-pair:v1|wss://gateway.example|",
            "mobius-pair:v1|wss://gateway.example|code with spaces",
            "mobius-pair:v1|wss://gateway.example|café",
            "mobius-pair:v1|wss://gateway.example|code|extra",
            "mobius-pair:v1|wss://gateway.example|\(String(repeating: "a", count: 513))",
        ]

        for value in invalid {
            XCTAssertThrowsError(try GatewayPairingSetup(value))
        }
        XCTAssertThrowsError(
            try GatewayPairingSetup("mobius-pair:v1|tcp://gateway.example:9191|code")
        ) { error in
            XCTAssertEqual(error as? GatewayWireError, .insecureRemoteEndpoint)
        }
    }

}
