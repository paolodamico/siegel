import CryptoKit
import Foundation
import XCTest

@testable import Siegel

// `siegel_fill_bolt` is a hand-written `extern "C"` symbol in the cdylib and
// is intentionally outside the BoltFFI-generated surface.
//
// The C ABI is `i32 siegel_fill_bolt(u64, *const u8, usize)`, which maps to
// `Int32 (UInt64, UnsafePointer<UInt8>?, Int)` in Swift.
@_silgen_name("siegel_fill_bolt")
private func siegel_fill_bolt(_ handle: UInt64, _ src: UnsafePointer<UInt8>?, _ len: Int) -> Int32

// `siegel_take_bolt` is the reverse: `i32 siegel_take_bolt(u64, *mut u8, usize)`.
@_silgen_name("siegel_take_bolt")
private func siegel_take_bolt(
    _ handle: UInt64, _ dst: UnsafeMutablePointer<UInt8>?, _ len: Int
) -> Int32

// Match the constants exported from `siegel::session`.
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
        siegel_fill_bolt(session.handleId(), buf.baseAddress, buf.count)
    }
}

// `siegel_take_bolt` into a native buffer, returning the code and a copy of the bytes for
// assertions. Like an app must, it zeroizes the buffer once done (`memset_s`, which the
// compiler can't drop) before freeing it.
private func take(_ session: SiegelSession, _ len: Int) -> (Int32, [UInt8]) {
    let buf = UnsafeMutableBufferPointer<UInt8>.allocate(capacity: len)
    defer {
        _ = memset_s(buf.baseAddress, len, 0, len)
        buf.deallocate()
    }
    buf.initialize(repeating: SENTINEL)
    let rc = siegel_take_bolt(session.handleId(), buf.baseAddress, len)
    return (rc, Array(buf))
}

// A session holding SHA-256 of `secret`, written by Rust and ready to take.
private func resealed(_ secret: [UInt8]) throws -> SiegelSession {
    let input = try SiegelSession(len: UInt32(secret.count))
    XCTAssertEqual(fill(input, secret), FILL_OK)
    return try sha256Reseal(input: input)
}

final class SiegelSessionTests: XCTestCase {

    func testNewSessionIsNotConsumed() throws {
        let session = try SiegelSession(len: 32)
        XCTAssertFalse(session.isConsumed())
        XCTAssertEqual(session.handleId(), session.handleId())
    }

    func testConsumedSessionReportsUnlocked() throws {
        // `isLocked()` is best-effort: a live session may be locked or not
        // depending on the platform's mlock support. A consumed session holds
        // no memory and must always report unlocked — deterministic anywhere.
        let session = try SiegelSession(len: 8)
        session.obliviate()
        XCTAssertFalse(session.isLocked())
    }

    func testZeroLengthRejected() {
        XCTAssertThrowsError(try SiegelSession(len: 0)) { error in
            guard case SessionError.invalidLength = error else {
                return XCTFail("expected invalidLength, got \(error)")
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
        XCTAssertEqual(siegel_fill_bolt(session.handleId(), nil, 8), FILL_ERR_NULL_SRC)
    }

    func testFillRejectsUnknownHandle() {
        let bytes = [UInt8](repeating: 0, count: 4)
        let rc = bytes.withUnsafeBufferPointer { buf in
            siegel_fill_bolt(0xDEAD, buf.baseAddress, buf.count)
        }
        XCTAssertEqual(rc, FILL_ERR_INVALID_HANDLE)
    }

    func testFillRejectsZeroHandle() {
        let bytes = [UInt8](repeating: 0, count: 4)
        let rc = bytes.withUnsafeBufferPointer { buf in
            siegel_fill_bolt(0, buf.baseAddress, buf.count)
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
            guard case SessionError.invalidState = error else {
                return XCTFail("expected invalidState, got \(error)")
            }
        }
        XCTAssertFalse(session.isConsumed())
        // After an InvalidState error the session is still fillable.
        XCTAssertEqual(fill(session, [UInt8](repeating: 2, count: 8)), FILL_OK)
        _ = try sha256Consume(session: session)
    }

    func testSecondConsumeRejected() throws {
        let session = try SiegelSession(len: 8)
        XCTAssertEqual(fill(session, [UInt8](repeating: 3, count: 8)), FILL_OK)
        _ = try sha256Consume(session: session)
        XCTAssertThrowsError(try sha256Consume(session: session)) { error in
            guard case SessionError.consumed = error else {
                return XCTFail("expected consumed, got \(error)")
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
            guard case SessionError.consumed = error else {
                return XCTFail("expected consumed, got \(error)")
            }
        }
    }

    func testDistinctSessionsHaveDistinctHandles() throws {
        let a = try SiegelSession(len: 8)
        let b = try SiegelSession(len: 8)
        XCTAssertNotEqual(a.handleId(), b.handleId())
    }

    func testLenReportsAllocatedCapacity() throws {
        let session = try SiegelSession(len: 64)
        XCTAssertEqual(session.len(), 64)
        XCTAssertEqual(fill(session, [UInt8](repeating: 7, count: 64)), FILL_OK)
        XCTAssertEqual(session.len(), 64)
    }

    func testReleasedSessionInvalidatesItsHandle() throws {
        var handle: UInt64 = 0
        do {
            let session = try SiegelSession(len: 8)
            handle = session.handleId()
            // Sanity: the handle resolves while the session is alive.
            XCTAssertEqual(fillRaw(handle, [UInt8](repeating: 0, count: 8)), FILL_OK)
        }
        // Swift's ARC is deterministic, so the session is gone here.
        let rc = fillRaw(handle, [UInt8](repeating: 0, count: 8))
        XCTAssertEqual(rc, FILL_ERR_INVALID_HANDLE)
    }

    func testResealThenTakeReturnsSha256() throws {
        let secret = [UInt8](repeating: 0x42, count: 16)
        let output = try resealed(secret)
        XCTAssertFalse(output.isConsumed())
        XCTAssertEqual(output.len(), 32)

        let (rc, digest) = take(output, 32)
        XCTAssertEqual(rc, TAKE_OK)
        XCTAssertEqual(digest, [UInt8](SHA256.hash(data: Data(secret))))
        XCTAssertTrue(output.isConsumed())
    }

    func testWrittenSessionCanBePassedBackIntoRust() throws {
        let secret = [UInt8](repeating: 0x42, count: 16)
        let output = try resealed(secret)

        let digest = try sha256Consume(session: output)
        XCTAssertEqual(digest, Data(SHA256.hash(data: Data(SHA256.hash(data: Data(secret))))))
        XCTAssertEqual(take(output, 32).0, TAKE_ERR_WRONG_STATE)
    }

    func testTakeRejectsNullDst() throws {
        let output = try resealed([UInt8](repeating: 1, count: 8))
        XCTAssertEqual(siegel_take_bolt(output.handleId(), nil, 32), TAKE_ERR_NULL_DST)
        XCTAssertFalse(output.isConsumed())
    }

    func testTakeRejectsUnknownHandle() {
        var out = [UInt8](repeating: 0, count: 4)
        let rc = out.withUnsafeMutableBufferPointer { buf in
            siegel_take_bolt(0xDEAD, buf.baseAddress, buf.count)
        }
        XCTAssertEqual(rc, TAKE_ERR_INVALID_HANDLE)
    }

    func testTakeRejectsLengthMismatchAndRecovers() throws {
        let output = try resealed([UInt8](repeating: 2, count: 8))
        let (rc, bytes) = take(output, 16)
        XCTAssertEqual(rc, TAKE_ERR_LEN_MISMATCH)
        XCTAssertEqual(bytes, [UInt8](repeating: SENTINEL, count: 16))
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
        let output = try resealed([UInt8](repeating: 3, count: 8))
        XCTAssertEqual(take(output, 32).0, TAKE_OK)

        let (rc, bytes) = take(output, 32)
        XCTAssertEqual(rc, TAKE_ERR_WRONG_STATE)
        XCTAssertEqual(bytes, [UInt8](repeating: SENTINEL, count: 32))
    }

    func testResealRejectsEmptyInput() throws {
        let input = try SiegelSession(len: 8)
        XCTAssertThrowsError(try sha256Reseal(input: input)) { error in
            guard case SessionError.invalidState = error else {
                return XCTFail("expected invalidState, got \(error)")
            }
        }
        XCTAssertFalse(input.isConsumed())
    }
}

private func fillRaw(_ handle: UInt64, _ bytes: [UInt8]) -> Int32 {
    bytes.withUnsafeBufferPointer { buf in
        siegel_fill_bolt(handle, buf.baseAddress, buf.count)
    }
}
