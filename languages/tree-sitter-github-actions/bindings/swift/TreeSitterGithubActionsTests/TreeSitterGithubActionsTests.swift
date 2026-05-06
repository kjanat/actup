import XCTest
import SwiftTreeSitter
import TreeSitterGithubActions

final class TreeSitterGithubActionsTests: XCTestCase {
    func testCanLoadGrammar() throws {
        let parser = Parser()
        let language = Language(language: tree_sitter_github_actions())
        XCTAssertNoThrow(try parser.setLanguage(language),
                         "Error loading Github Actions grammar")
    }
}
