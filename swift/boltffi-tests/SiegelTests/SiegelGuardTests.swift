import Foundation
import XCTest

@testable import Siegel

/// The fork happens inside Rust. Swift on iOS hides `fork(2)`.
@_silgen_name("unsafe_test_only_siegel_front_guard_bolt")
private func frontGuardSegFault(_ handle: UInt64) -> Int32

// POSIX signal numbers. Linux and macOS both use these values for SIGSEGV.
// SIGBUS differs (10 on macOS, 7 on Linux).
private let SIGSEGV_CODE: Int32 = 11
private let SIGBUS_LINUX: Int32 = 7
private let SIGBUS_DARWIN: Int32 = 10

final class SiegelGuardTests: XCTestCase {

    func testReadingProtectedMemorySegfaults() throws {
        let session = try SiegelSession(len: 64)
        let signal = frontGuardSegFault(session.handleId())
        XCTAssertTrue(
            signal == SIGSEGV_CODE || signal == SIGBUS_LINUX || signal == SIGBUS_DARWIN,
            "expected SIGSEGV (\(SIGSEGV_CODE)) or SIGBUS "
                + "(\(SIGBUS_LINUX)/\(SIGBUS_DARWIN)), got \(signal)"
        )
    }
}
