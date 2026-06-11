import XCTest

import KmuxBindings

/// Smoke tests for the uniffi boundary itself (no live driver needed).
final class FfiBoundaryTests: XCTestCase {
    /// Calling any exported fn forces uniffi's contract-version + per-function
    /// checksum check (fatalError on a bindings/dylib mismatch) — that's the
    /// real boundary guard. Asserting the ABI is non-zero keeps this a pure
    /// smoke test without pinning a number that drifts on every ABI bump.
    func testFfiBoundaryInitializes() {
        XCTAssertGreaterThan(kmuxFfiAbiVersion(), 0)
    }
}
