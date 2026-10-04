import AppKit
import XCTest
@testable import Kit

final class TranscriptMediaTests: XCTestCase {
    func testDerivesInlineAndResourceMedia() throws {
        let bytes = Data([1, 2, 3])
        let media = try XCTUnwrap(TranscriptMedia.derive(.image(data: bytes.base64EncodedString(), mimeType: "image/png", uri: nil)))
        XCTAssertEqual(try media.boundedData(), bytes)
        XCTAssertNotNil(TranscriptMedia.derive(.resource(uri: "file:///tmp/image.png", mimeType: "image/png", text: nil, blob: nil)))
        XCTAssertNotNil(TranscriptMedia.derive(.resourceLink(uri: "https://example.com/audio.wav", name: "Audio", mimeType: "audio/wav")))
        XCTAssertNil(TranscriptMedia.derive(.text("hello")))
    }

    func testUnsafeLinksAndOversizedDataAreRejected() throws {
        for value in ["javascript:alert(1)", "data:image/png;base64,AA==", "file:///tmp/run.command", "file://remote/tmp/image.png"] {
            XCTAssertNil(TranscriptMedia.safeURL(URL(string: value)))
        }
        XCTAssertNotNil(TranscriptMedia.safeURL(URL(string: "https://example.com/image.png")))
        let large = TranscriptMedia(mimeType: "image/png", data: Data(count: TranscriptMedia.maximumBytes + 1))
        XCTAssertNil(try large.boundedData())
        XCTAssertThrowsError(try ClipboardMediaImport.writeImage(Data([1, 2])))
    }

    func testRemotePreviewRequiresConsentAndOpenDoesNotFetch() async throws {
        let media = TranscriptMedia(mimeType: "image/png", url: URL(string: "http://127.0.0.1:1/tracker.png"))
        XCTAssertTrue(media.requiresRemotePreviewConsent)
        let cache = TranscriptMediaCache()
        let thumbnail = await cache.thumbnail(media, identity: TranscriptMediaIdentity(owner: UUID()))
        XCTAssertNil(thumbnail)
        let openURL = await cache.openURL(media)
        XCTAssertEqual(openURL, media.url)
        let inline = TranscriptMedia(mimeType: "image/png", data: Data([1]), url: media.url)
        XCTAssertFalse(inline.requiresRemotePreviewConsent)
    }

    func testViewIdentityDoesNotComparePayloadsAndRevisionInvalidates() {
        let id = TranscriptMediaIdentity(owner: UUID())
        let first = TranscriptMediaView(media: TranscriptMedia(mimeType: "image/png", data: Data([1])), identity: id)
        let second = TranscriptMediaView(media: TranscriptMedia(mimeType: "image/png", data: Data([2])), identity: id)
        XCTAssertEqual(first, second)
        let changed = TranscriptMediaView(media: second.media, identity: id, revision: 1)
        XCTAssertNotEqual(first, changed)
        XCTAssertNotEqual(id, TranscriptMediaIdentity(owner: id.owner, block: 1))
    }

    func testClipboardImportAndThumbnailDownsampling() async throws {
        let bitmap = try XCTUnwrap(NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 1200, pixelsHigh: 800,
            bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
            colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
        ))
        let data = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
        let url = try ClipboardMediaImport.writeImage(data)
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        let media = TranscriptMedia(mimeType: "image/png", url: url)
        XCTAssertNotNil(try media.boundedData())
        let cache = TranscriptMediaCache()
        let identity = TranscriptMediaIdentity(owner: UUID())
        let first = await cache.thumbnail(media, identity: identity)
        let image = try XCTUnwrap(first)
        XCTAssertEqual(image.width, 640)
        XCTAssertLessThanOrEqual(image.height, 640)
        let second = await cache.thumbnail(media, identity: identity)
        XCTAssertTrue(image === second)
        // Replacements at the same source identity must never return a stale image.
        let invalid = TranscriptMedia(mimeType: "image/png", data: Data([1, 2]))
        let replaced = await cache.thumbnail(invalid, identity: identity)
        XCTAssertNil(replaced)
    }
}
