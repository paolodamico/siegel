import Foundation
import XCTest

@testable import Siegel

/// The fork happens inside Rust. Swift on iOS hides `fork(2)`.
@_silgen_name("unsafe_test_only_siegel_front_guard_bolt")
private func frontGuardSegFault(_ handle: UInt64) -> Int32

// Darwin signal numbers: 7 is SIGEMT here, not SIGBUS, so accepting it would let
// an unrelated child termination satisfy the assertion.
private let SIGSEGV_CODE: Int32 = 11
private let SIGBUS_CODE: Int32 = 10

final class SiegelGuardTests: XCTestCase {

    func testReadingProtectedMemorySegfaults() throws {
        let session = try SiegelSession(len: 64)
        let signal = frontGuardSegFault(session.handleId())
        XCTAssertTrue(
            signal == SIGSEGV_CODE || signal == SIGBUS_CODE,
            "expected SIGSEGV (\(SIGSEGV_CODE)) or SIGBUS (\(SIGBUS_CODE)), got \(signal)"
        )
    }
}
