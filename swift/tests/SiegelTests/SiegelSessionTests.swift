import CryptoKit
import Foundation
import XCTest

@testable import Siegel

// `siegel_fill` is a hand-written `extern "C"` symbol in the cdylib and
// is intentionally outside the UniFFI scaffolding. Bind to it directly
// via the linker. The C ABI is `i32 siegel_fill(u64, *const u8, usize)`,
// which maps to `Int32 (UInt64, UnsafePointer<UInt8>?, Int)` in Swift.
@_silgen_name("siegel_fill")
private func siegel_fill(_ handle: UInt64, _ src: UnsafePointer<UInt8>?, _ len: Int) -> Int32

// Match the constants exported from `siegel-uniffi::session`.
private let FILL_OK: Int32 = 0
private let FILL_ERR_INVALID_HANDLE: Int32 = -1
private let FILL_ERR_LEN_MISMATCH: Int32 = -2
private let FILL_ERR_NULL_SRC: Int32 = -3
private let FILL_ERR_WRONG_STATE: Int32 = -4

private func fill(_ session: SiegelSession, _ bytes: [UInt8]) -> Int32 {
    bytes.withUnsafeBufferPointer { buf in
        siegel_fill(session.handleId(), buf.baseAddress, buf.count)
    }
}

final class SiegelSessionTests: XCTestCase {

    func testNewSessionIsNotConsumed() throws {
        let session = try SiegelSession(len: 32)
        XCTAssertFalse(session.isConsumed())
        XCTAssertEqual(session.handleId(), session.handleId())
    }

    func testZeroLengthRejected() {
        XCTAssertThrowsError(try SiegelSession(len: 0)) { error in
            guard case SessionError.InvalidLength = error else {
                return XCTFail("expected InvalidLength, got \(error)")
            }
        }
    }

    func testFillAndConsumeReturnsSha256() throws {
        let secret = [UInt8](repeating: 0x42, count: 16)
        let session = try SiegelSession(len: 16)

        XCTAssertEqual(fill(session, secret), FILL_OK)

        let digest = try sha256Consume(session: session)
        let expected = Data(SHA256.hash(data: Data(secret)))

        XCTAssertEqual(digest, expected)
        XCTAssertTrue(session.isConsumed())
    }

    func testFillRejectsNullSrc() throws {
        let session = try SiegelSession(len: 8)
        XCTAssertEqual(siegel_fill(session.handleId(), nil, 8), FILL_ERR_NULL_SRC)
    }

    func testFillRejectsUnknownHandle() {
        let bytes = [UInt8](repeating: 0, count: 4)
        let rc = bytes.withUnsafeBufferPointer { buf in
            siegel_fill(0xDEAD, buf.baseAddress, buf.count)
        }
        XCTAssertEqual(rc, FILL_ERR_INVALID_HANDLE)
    }

    func testFillRejectsLengthMismatchAndRecovers() throws {
        let session = try SiegelSession(len: 16)
        XCTAssertEqual(fill(session, [UInt8](repeating: 0, count: 8)), FILL_ERR_LEN_MISMATCH)
        // Session is still usable for a correct-length fill.
        XCTAssertEqual(fill(session, [UInt8](repeating: 0, count: 16)), FILL_OK)
    }

    func testDoubleFillRejected() throws {
        let session = try SiegelSession(len: 8)
        XCTAssertEqual(fill(session, [UInt8](repeating: 1, count: 8)), FILL_OK)
        XCTAssertEqual(fill(session, [UInt8](repeating: 1, count: 8)), FILL_ERR_WRONG_STATE)
    }

    func testConsumeBeforeFillIsInvalidState() throws {
        let session = try SiegelSession(len: 8)
        XCTAssertThrowsError(try sha256Consume(session: session)) { error in
            guard case SessionError.InvalidState = error else {
                return XCTFail("expected InvalidState, got \(error)")
            }
        }
        XCTAssertFalse(session.isConsumed())
        // After an InvalidState error the session is still fillable.
        XCTAssertEqual(fill(session, [UInt8](repeating: 2, count: 8)), FILL_OK)
        _ = try sha256Consume(session: session)
    }

    func testSecondConsumeIsRejected() throws {
        let session = try SiegelSession(len: 8)
        XCTAssertEqual(fill(session, [UInt8](repeating: 3, count: 8)), FILL_OK)
        _ = try sha256Consume(session: session)

        XCTAssertThrowsError(try sha256Consume(session: session)) { error in
            guard case SessionError.Consumed = error else {
                return XCTFail("expected Consumed, got \(error)")
            }
        }
    }

    func testFillAfterConsumeRejected() throws {
        let session = try SiegelSession(len: 8)
        XCTAssertEqual(fill(session, [UInt8](repeating: 4, count: 8)), FILL_OK)
        _ = try sha256Consume(session: session)
        XCTAssertEqual(fill(session, [UInt8](repeating: 4, count: 8)), FILL_ERR_WRONG_STATE)
    }

    func testObliviateWipesAndIsIdempotent() throws {
        let session = try SiegelSession(len: 16)
        XCTAssertEqual(fill(session, [UInt8](repeating: 5, count: 16)), FILL_OK)

        session.obliviate()
        XCTAssertTrue(session.isConsumed())
        session.obliviate()  // no-op the second time

        XCTAssertThrowsError(try sha256Consume(session: session)) { error in
            guard case SessionError.Consumed = error else {
                return XCTFail("expected Consumed, got \(error)")
            }
        }
    }

    func testDistinctSessionsHaveDistinctHandles() throws {
        let a = try SiegelSession(len: 8)
        let b = try SiegelSession(len: 8)
        XCTAssertNotEqual(a.handleId(), b.handleId())
    }

    func testHandleInvalidatedAfterSessionDrop() throws {
        // Hold the handle past the session's lifetime. ARC drops `session`
        // when the inner scope exits; the registry stores only a Weak so
        // subsequent fills must fail.
        var handle: UInt64 = 0
        autoreleasepool {
            // swiftlint:disable:next force_try
            let session = try! SiegelSession(len: 8)
            handle = session.handleId()
        }
        let bytes = [UInt8](repeating: 0, count: 8)
        let rc = bytes.withUnsafeBufferPointer { buf in
            siegel_fill(handle, buf.baseAddress, buf.count)
        }
        XCTAssertEqual(rc, FILL_ERR_INVALID_HANDLE)
    }
}
