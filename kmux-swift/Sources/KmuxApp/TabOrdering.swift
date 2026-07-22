/// Resolve user-facing tab positions against the order shown in the tab strip.
/// Tab IDs are persistent server identifiers and may be sparse after closes and
/// reorders, so they must never be treated as display positions.
func tabID(atDisplayedPosition position: Int, in orderedIDs: [UInt32]) -> UInt32? {
    guard orderedIDs.indices.contains(position) else { return nil }
    return orderedIDs[position]
}

/// Return the tab reached by cycling `offset` places through the displayed
/// order, wrapping at either end.
func cycledTabID(current: UInt32?, offset: Int, in orderedIDs: [UInt32]) -> UInt32? {
    guard !orderedIDs.isEmpty else { return nil }
    let currentPosition = current.flatMap { orderedIDs.firstIndex(of: $0) } ?? 0
    let destination = (currentPosition + offset).modulo(orderedIDs.count)
    return orderedIDs[destination]
}

private extension Int {
    func modulo(_ divisor: Int) -> Int {
        let remainder = self % divisor
        return remainder >= 0 ? remainder : remainder + divisor
    }
}
