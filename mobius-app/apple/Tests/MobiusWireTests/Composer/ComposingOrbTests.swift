@testable import Mobius
import XCTest

final class ComposingOrbTests: XCTestCase {
    func testComposingOrbMatchesThinkingOrbsReferenceFrame() throws {
        let dots = MobiusComposingOrbRenderer.dots(at: 0.95)
        let first = try XCTUnwrap(dots.first)
        let last = try XCTUnwrap(dots.last)

        XCTAssertEqual(dots.count, 566)
        XCTAssertEqual(first.x, 37.322, accuracy: 0.001)
        XCTAssertEqual(first.y, 30.656, accuracy: 0.001)
        XCTAssertEqual(first.radius, 0.317, accuracy: 0.001)
        XCTAssertEqual(last.x, 38.864, accuracy: 0.001)
        XCTAssertEqual(last.y, 29.054, accuracy: 0.001)
        XCTAssertNotEqual(dots, MobiusComposingOrbRenderer.dots(at: 1.05))
    }
}
