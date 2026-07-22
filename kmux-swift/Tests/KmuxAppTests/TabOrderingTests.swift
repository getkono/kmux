import XCTest

@testable import KmuxApp

final class TabOrderingTests: XCTestCase {
    func testDisplayedPositionResolvesSparseTabID() {
        let tabIDs: [UInt32] = [0, 3, 4, 5, 6, 7, 8]

        XCTAssertEqual(tabID(atDisplayedPosition: 0, in: tabIDs), 0)
        XCTAssertEqual(tabID(atDisplayedPosition: 1, in: tabIDs), 3)
        XCTAssertEqual(tabID(atDisplayedPosition: 6, in: tabIDs), 8)
        XCTAssertNil(tabID(atDisplayedPosition: 7, in: tabIDs))
    }

    func testCycleFollowsDisplayedOrderAndWraps() {
        let tabIDs: [UInt32] = [8, 3, 6]

        XCTAssertEqual(cycledTabID(current: 8, offset: 1, in: tabIDs), 3)
        XCTAssertEqual(cycledTabID(current: 6, offset: 1, in: tabIDs), 8)
        XCTAssertEqual(cycledTabID(current: 8, offset: -1, in: tabIDs), 6)
    }

    func testCycleHandlesMissingSelectionAndNoTabs() {
        XCTAssertEqual(cycledTabID(current: 99, offset: 1, in: [4, 7]), 7)
        XCTAssertNil(cycledTabID(current: nil, offset: 1, in: []))
    }
}
