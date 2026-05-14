import Darwin
import Foundation
import XCTest

@testable import Siegel

// Forks a child inside Rust (Swift's iOS Darwin module hides `fork(2)`),
// has the child read one byte from the front guard page, then returns the
// terminating signal observed by the parent — `SIGSEGV` or `SIGBUS` when
// the protection is in force.
//
// One foreign segfault test is enough to verify the binding boundary is
// wired up. The full matrix of front/back guards + sealed data + canary
// is covered by Rust unit tests in the `siegel` crate.
@_silgen_name("unsafe_test_only_siegel_front_guard_seg_fault")
private func unsafe_test_only_siegel_front_guard_seg_fault(_ handle: UInt64) -> Int32

final class SiegelGuardTests: XCTestCase {

    func testReadingProtectedMemorySegfaults() throws {
        let session = try SiegelSession(len: 64)
        let signal = unsafe_test_only_siegel_front_guard_seg_fault(session.handleId())
        XCTAssertTrue(
            signal == SIGSEGV || signal == SIGBUS,
            "expected SIGSEGV (\(SIGSEGV)) or SIGBUS (\(SIGBUS)), got \(signal)"
        )
    }
}
