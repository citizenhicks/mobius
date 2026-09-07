import Foundation
@testable import Mobius
import XCTest

final class FileTreeNodeTests: XCTestCase {
    func testNestsPathsFoldersFirst() {
        let tree = FileTreeNode.tree(from: [
            WorkspaceFileRecord(path: "src/main.rs", size: 10),
            WorkspaceFileRecord(path: "src/lib/mod.rs", size: 20),
            WorkspaceFileRecord(path: "README.md", size: 30),
            WorkspaceFileRecord(path: "Cargo.toml", size: 40)
        ])

        XCTAssertEqual(tree.map(\.name), ["src", "Cargo.toml", "README.md"])
        XCTAssertNil(tree[0].size)
        XCTAssertEqual(tree[1].size, 40)
        XCTAssertNil(tree[1].children)

        let src = try? XCTUnwrap(tree[0].children)
        XCTAssertEqual(src?.map(\.name), ["lib", "main.rs"])
        XCTAssertEqual(src?[0].id, "src/lib")
        XCTAssertEqual(src?[1].id, "src/main.rs")
        XCTAssertEqual(src?[0].children?.map(\.name), ["mod.rs"])
    }
}
