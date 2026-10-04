import AppKit
import ImageIO
import SwiftUI
import UniformTypeIdentifiers

/// Value identity keeps thumbnail work independent of streaming text revisions.
struct TranscriptMedia: Equatable {
    static let maximumBytes = 10 * 1024 * 1024
    let mimeType: String
    var name: String? = nil
    var base64: String? = nil
    var data: Data? = nil
    var url: URL? = nil

    var isImage: Bool { mimeType.hasPrefix("image/") }
    var label: String { name ?? (isImage ? "Image attachment" : "Audio attachment") }
    var requiresRemotePreviewConsent: Bool {
        data == nil && base64 == nil && Self.safeURL(url).map { !$0.isFileURL } == true
    }

    static func safeURL(_ url: URL?) -> URL? {
        guard let url else { return nil }
        if ["https", "http"].contains(url.scheme?.lowercased() ?? ""), url.host != nil { return url }
        guard url.isFileURL, url.host == nil || url.host == "" || url.host == "localhost",
              let type = UTType(filenameExtension: url.pathExtension),
              type.conforms(to: .image) || type.conforms(to: .audio) else { return nil }
        return url
    }

    static func derive(_ block: DesktopContentBlock) -> TranscriptMedia? {
        switch block {
        case .image(let data, let mime, let uri):
            return TranscriptMedia(mimeType: mime ?? "image/*", base64: data, url: uri.flatMap(URL.init(string:)))
        case .audio(let data, let mime):
            return TranscriptMedia(mimeType: mime ?? "audio/*", base64: data)
        case .resourceLink(let uri, let name, let mime):
            guard let mime = mime ?? inferredMIME(uri),
                  mime.hasPrefix("image/") || mime.hasPrefix("audio/") else { return nil }
            return TranscriptMedia(mimeType: mime, name: name, url: URL(string: uri))
        case .resource(let uri, let mime, _, let blob):
            guard let mime = mime ?? inferredMIME(uri),
                  mime.hasPrefix("image/") || mime.hasPrefix("audio/") else { return nil }
            return TranscriptMedia(mimeType: mime, base64: blob, url: uri.flatMap(URL.init(string:)))
        default: return nil
        }
    }

    private static func inferredMIME(_ uri: String?) -> String? {
        guard let uri, let url = URL(string: uri) else { return nil }
        return UTType(filenameExtension: url.pathExtension)?.preferredMIMEType
    }

    func boundedData() throws -> Data? {
        if let data { return data.count <= Self.maximumBytes ? data : nil }
        if let base64 {
            guard base64.utf8.count <= (Self.maximumBytes + 2) / 3 * 4,
                  let decoded = Data(base64Encoded: base64), decoded.count <= Self.maximumBytes else { return nil }
            return decoded
        }
        guard let url = Self.safeURL(url), url.isFileURL else { return nil }
        let values = try url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
        guard values.isRegularFile == true, let size = values.fileSize, size <= Self.maximumBytes else { return nil }
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        let bytes = try handle.read(upToCount: Self.maximumBytes + 1)
        return bytes.flatMap { $0.count <= Self.maximumBytes ? $0 : nil }
    }
}

/// Actor isolation confines decoding and its bounded cache off the UI actor.
actor TranscriptMediaCache {
    static let shared = TranscriptMediaCache()
    private var thumbnails: [TranscriptMediaIdentity: (media: TranscriptMedia, image: CGImage)] = [:]
    private var order: [TranscriptMediaIdentity] = []

    func thumbnail(
        _ media: TranscriptMedia, identity: TranscriptMediaIdentity, allowRemote: Bool = false
    ) async -> CGImage? {
        // Even a previously cached remote preview requires consent in this view.
        guard media.isImage, allowRemote || !media.requiresRemotePreviewConsent else { return nil }
        // Payload equality happens only on this actor, never in SwiftUI's task/equality keys.
        if let cached = thumbnails[identity], cached.media == media { return cached.image }
        let bytes: Data?
        if media.data == nil, media.base64 == nil,
           let url = TranscriptMedia.safeURL(media.url), !url.isFileURL {
            bytes = await remoteImageData(url)
        } else { bytes = try? media.boundedData() }
        guard let data = bytes,
              let source = CGImageSourceCreateWithData(data as CFData, nil),
              let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceThumbnailMaxPixelSize: 640,
                kCGImageSourceShouldCacheImmediately: true,
              ] as CFDictionary) else { return nil }
        while order.count >= 16 { thumbnails.removeValue(forKey: order.removeFirst()) }
        order.removeAll { $0 == identity }
        thumbnails[identity] = (media, image)
        order.append(identity)
        return image
    }

    private func remoteImageData(_ url: URL) async -> Data? {
        do {
            var request = URLRequest(url: url)
            request.timeoutInterval = 20
            let (stream, response) = try await URLSession.shared.bytes(for: request)
            guard let response = response as? HTTPURLResponse,
                  (200..<300).contains(response.statusCode),
                  TranscriptMedia.safeURL(response.url) != nil,
                  response.mimeType?.hasPrefix("image/") == true,
                  response.expectedContentLength <= Int64(TranscriptMedia.maximumBytes) else { return nil }
            var data = Data()
            for try await byte in stream {
                guard data.count < TranscriptMedia.maximumBytes, !Task.isCancelled else { return nil }
                data.append(byte)
            }
            return data
        } catch { return nil }
    }

    func openURL(_ media: TranscriptMedia) -> URL? {
        if let url = TranscriptMedia.safeURL(media.url) {
            if !url.isFileURL { return url }
            // Do not let a media-looking symlink launch an application or script.
            guard TranscriptMedia.safeURL(url.resolvingSymlinksInPath()) != nil,
                  (try? media.boundedData()) != nil else { return nil }
            return url
        }
        guard let data = try? media.boundedData() else { return nil }
        let imageType = CGImageSourceCreateWithData(data as CFData, nil)
            .flatMap { CGImageSourceGetType($0) }.flatMap { UTType($0 as String) }
        guard let type = imageType ?? UTType(mimeType: media.mimeType),
              type.conforms(to: .image) || type.conforms(to: .audio),
              let ext = type.preferredFilenameExtension else { return nil }
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("Kit-media-\(UUID().uuidString).\(ext)")
        do { try data.write(to: url, options: .atomic); return url } catch { return nil }
    }
}

/// Stable, small keys never hash the inline image/audio payload.
struct TranscriptMediaIdentity: Hashable {
    let owner: UUID
    var block: Int = 0
}

private struct TranscriptMediaLoadIdentity: Equatable {
    let source: TranscriptMediaIdentity
    let revision: UInt64
    let allowRemote: Bool
}

struct TranscriptMediaView: View, Equatable {
    let media: TranscriptMedia
    let identity: TranscriptMediaIdentity
    var revision: UInt64 = 0
    @State private var thumbnail: CGImage?
    @State private var loaded = false
    @State private var opening = false
    @State private var openFailed = false
    @State private var consentedRemoteURL: URL?
    private var allowRemote: Bool {
        guard let url = TranscriptMedia.safeURL(media.url) else { return false }
        return consentedRemoteURL == url
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.identity == rhs.identity && lhs.revision == rhs.revision
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button {
                opening = true
                Task {
                    if let url = await TranscriptMediaCache.shared.openURL(media) {
                        openFailed = !NSWorkspace.shared.open(url)
                    } else { openFailed = true }
                    opening = false
                }
            } label: {
                VStack(alignment: .leading, spacing: 4) {
                    if let thumbnail {
                        Image(decorative: thumbnail, scale: 1).resizable().scaledToFit()
                            .frame(maxWidth: 320, maxHeight: 240)
                            .clipShape(RoundedRectangle(cornerRadius: 8))
                    } else if !loaded && media.isImage && (!media.requiresRemotePreviewConsent || allowRemote) {
                        ProgressView().frame(width: 80, height: 60)
                    }
                    Label(media.label, systemImage: media.isImage ? "photo" : "waveform")
                        .font(.caption).lineLimit(2)
                    if opening { ProgressView().controlSize(.small) }
                    if openFailed { Text("Attachment unavailable").font(.caption).foregroundStyle(.secondary) }
                }
            }
            .buttonStyle(.plain)
            .disabled(opening)
            .accessibilityLabel("Open \(media.label)")
            if media.isImage && media.requiresRemotePreviewConsent && !allowRemote {
                Button("Load remote preview") { consentedRemoteURL = TranscriptMedia.safeURL(media.url) }
                    .font(.caption)
                    .help("Contacts the remote server to load this image")
            }
        }
        .task(id: TranscriptMediaLoadIdentity(source: identity, revision: revision, allowRemote: allowRemote)) {
            loaded = false
            let image = await TranscriptMediaCache.shared.thumbnail(media, identity: identity, allowRemote: allowRemote)
            guard !Task.isCancelled else { return }
            thumbnail = image
            loaded = true
        }
    }
}

/// The producer increments revision whenever blocks are replaced or appended.
/// Unrelated row updates compare only these scalar values, without enumerating blocks.
struct AssistantMediaView: View, Equatable {
    let entryID: UUID
    let revision: UInt64
    let blocks: [DesktopContentBlock]

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.entryID == rhs.entryID && lhs.revision == rhs.revision
    }

    var body: some View {
        ForEach(Array(blocks.enumerated()), id: \.offset) { index, block in
            if let media = TranscriptMedia.derive(block) {
                TranscriptMediaView(
                    media: media, identity: TranscriptMediaIdentity(owner: entryID, block: index), revision: revision
                ).equatable()
            }
        }
    }
}
