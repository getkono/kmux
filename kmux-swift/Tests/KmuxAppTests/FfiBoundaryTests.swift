import XCTest

import KmuxBindings

/// Smoke tests for the uniffi boundary itself (no live driver needed).
final class FfiBoundaryTests: XCTestCase {
    /// The Swift app asserts this on startup; pin it so a drifted ABI fails CI.
    func testAbiVersionMatches() {
        XCTAssertEqual(kmuxFfiAbiVersion(), 5)
    }
}
