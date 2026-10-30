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

// `siegel_take` is the reverse: `i32 siegel_take(u64, *mut u8, usize)`.
@_silgen_name("siegel_take")
private func siegel_take(_ handle: UInt64, _ dst: UnsafeMutablePointer<UInt8>?, _ len: Int) -> Int32

// Match the constants exported from `siegel-uniffi::session`.
private let FILL_OK: Int32 = 0
private let FILL_ERR_INVALID_HANDLE: Int32 = -1
private let FILL_ERR_LEN_MISMATCH: Int32 = -2
private let FILL_ERR_NULL_SRC: Int32 = -3
private let FILL_ERR_WRONG_STATE: Int32 = -4
private let TAKE_OK: Int32 = 0
private let TAKE_ERR_INVALID_HANDLE: Int32 = -1
private let TAKE_ERR_LEN_MISMATCH: Int32 = -2
private let TAKE_ERR_NULL_DST: Int32 = -3
private let TAKE_ERR_WRONG_STATE: Int32 = -4

// Arbitrary non-secret bytes; rejected takes must leave the destination holding these.
private let SENTINEL: UInt8 = 0xA5

private func fill(_ session: SiegelSession, _ bytes: [UInt8]) -> Int32 {
    bytes.withUnsafeBufferPointer { buf in
        siegel_fill(session.handleId(), buf.baseAddress, buf.count)
    }
}

// `siegel_take` into a native buffer, returning the code and a copy of the bytes for
// assertions. Like an app must, it zeroizes the buffer once done (`memset_s`, which the
// compiler can't drop) before freeing it.
private func take(_ session: SiegelSession, _ len: Int) -> (Int32, [UInt8]) {
    let buf = UnsafeMutableBufferPointer<UInt8>.allocate(capacity: len)
    defer {
        _ = memset_s(buf.baseAddress, len, 0, len)
        buf.deallocate()
    }
    buf.initialize(repeating: SENTINEL)
    let rc = siegel_take(session.handleId(), buf.baseAddress, len)
    return (rc, Array(buf))
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

    func testResealThenTakeReturnsSha256() throws {
        let secret = [UInt8](repeating: 0x42, count: 16)
        let input = try SiegelSession(len: 16)
        XCTAssertEqual(fill(input, secret), FILL_OK)

        let output = try sha256Reseal(input: input)
        XCTAssertTrue(input.isConsumed())
        XCTAssertFalse(output.isConsumed())
        XCTAssertEqual(output.len(), 32)

        let (rc, digest) = take(output, 32)
        let expected = [UInt8](SHA256.hash(data: Data(secret)))

        XCTAssertEqual(rc, TAKE_OK)
        XCTAssertEqual(digest, expected)
        XCTAssertTrue(output.isConsumed())
    }

    func testWrittenSessionCanBePassedBackIntoRust() throws {
        let secret = [UInt8](repeating: 0x42, count: 16)
        let input = try SiegelSession(len: 16)
        XCTAssertEqual(fill(input, secret), FILL_OK)
        let output = try sha256Reseal(input: input)

        let digest = try sha256Consume(session: output)
        let expected = Data(SHA256.hash(data: Data(SHA256.hash(data: Data(secret)))))

        XCTAssertEqual(digest, expected)
        XCTAssertEqual(take(output, 32).0, TAKE_ERR_WRONG_STATE)
    }

    func testTakeRejectsNullDst() throws {
        let input = try SiegelSession(len: 8)
        XCTAssertEqual(fill(input, [UInt8](repeating: 1, count: 8)), FILL_OK)
        let output = try sha256Reseal(input: input)
        XCTAssertEqual(siegel_take(output.handleId(), nil, 32), TAKE_ERR_NULL_DST)
        XCTAssertFalse(output.isConsumed())
    }

    func testTakeRejectsUnknownHandle() {
        var out = [UInt8](repeating: 0, count: 4)
        let rc = out.withUnsafeMutableBufferPointer { buf in
            siegel_take(0xDEAD, buf.baseAddress, buf.count)
        }
        XCTAssertEqual(rc, TAKE_ERR_INVALID_HANDLE)
    }

    func testTakeRejectsLengthMismatchAndRecovers() throws {
        let input = try SiegelSession(len: 8)
        XCTAssertEqual(fill(input, [UInt8](repeating: 2, count: 8)), FILL_OK)
        let output = try sha256Reseal(input: input)
        XCTAssertEqual(take(output, 16).0, TAKE_ERR_LEN_MISMATCH)
        // Session is still takeable with a correctly sized buffer.
        XCTAssertEqual(take(output, 32).0, TAKE_OK)
    }

    func testTakeRejectsSessionFilledFromNative() throws {
        let secret = [UInt8](repeating: 6, count: 8)
        let session = try SiegelSession(len: 8)
        XCTAssertEqual(fill(session, secret), FILL_OK)

        let (rc, bytes) = take(session, 8)
        XCTAssertEqual(rc, TAKE_ERR_WRONG_STATE)
        XCTAssertEqual(bytes, [UInt8](repeating: SENTINEL, count: 8))
        // The filled secret is still there for Rust to consume.
        XCTAssertEqual(try sha256Consume(session: session), Data(SHA256.hash(data: Data(secret))))
    }

    func testTakeBeforeWriteIsRejected() throws {
        let session = try SiegelSession(len: 8)
        XCTAssertEqual(take(session, 8).0, TAKE_ERR_WRONG_STATE)
        XCTAssertFalse(session.isConsumed())
    }

    func testSecondTakeIsRejected() throws {
        let input = try SiegelSession(len: 8)
        XCTAssertEqual(fill(input, [UInt8](repeating: 3, count: 8)), FILL_OK)
        let output = try sha256Reseal(input: input)
        XCTAssertEqual(take(output, 32).0, TAKE_OK)

        let (rc, bytes) = take(output, 32)
        XCTAssertEqual(rc, TAKE_ERR_WRONG_STATE)
        XCTAssertEqual(bytes, [UInt8](repeating: SENTINEL, count: 32))
    }

    func testResealRejectsEmptyInput() throws {
        let input = try SiegelSession(len: 8)
        XCTAssertThrowsError(try sha256Reseal(input: input)) { error in
            guard case SessionError.InvalidState = error else {
                return XCTFail("expected InvalidState, got \(error)")
            }
        }
        XCTAssertFalse(input.isConsumed())
    }
}
