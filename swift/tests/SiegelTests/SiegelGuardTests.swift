import Darwin
import Foundation
import XCTest

@testable import Siegel

// These helpers fork a child inside Rust (Swift's iOS Darwin module hides
// `fork(2)`), have the child read into a guard / sealed page, then return
// the terminating signal observed by the parent (`SIGSEGV` or `SIGBUS`)
@_silgen_name("unsafe_test_only_siegel_front_guard_seg_fault")
private func unsafe_test_only_siegel_front_guard_seg_fault(_ handle: UInt64) -> Int32
@_silgen_name("unsafe_test_only_siegel_back_guard_seg_fault")
private func unsafe_test_only_siegel_back_guard_seg_fault(_ handle: UInt64) -> Int32
@_silgen_name("unsafe_test_only_siegel_sealed_data_seg_fault")
private func unsafe_test_only_siegel_sealed_data_seg_fault(_ handle: UInt64) -> Int32

final class SiegelGuardTests: XCTestCase {

    private func assertCrashSignal(
        _ signal: Int32,
        file: StaticString = #file,
        line: UInt = #line
    ) {
        XCTAssertTrue(
            signal == SIGSEGV || signal == SIGBUS,
            "expected SIGSEGV (\(SIGSEGV)) or SIGBUS (\(SIGBUS)), got \(signal)",
            file: file,
            line: line
        )
    }

    func testReadingFrontGuardPageSegfaults() throws {
        let session = try SiegelSession(len: 64)
        assertCrashSignal(unsafe_test_only_siegel_front_guard_seg_fault(session.handleId()))
    }

    func testReadingBackGuardPageSegfaults() throws {
        let session = try SiegelSession(len: 64)
        assertCrashSignal(unsafe_test_only_siegel_back_guard_seg_fault(session.handleId()))
    }

    /// The canary lives inside the protected data region. Once sealed
    /// (`PROT_NONE`, the at-rest state), any read into it — including the
    /// canary bytes — must trap.
    func testReadingSealedDataSegfaults() throws {
        let session = try SiegelSession(len: 64)
        assertCrashSignal(unsafe_test_only_siegel_sealed_data_seg_fault(session.handleId()))
    }
}
