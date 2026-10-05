import Foundation
import ImageIO
import UniformTypeIdentifiers

/// Clipboard bytes are copied on the AppKit thread; validation and conversion are not.
enum ClipboardMediaImport {
    static func writeImage(_ data: Data) throws -> URL {
        guard data.count <= TranscriptMedia.maximumBytes,
              let source = CGImageSourceCreateWithData(data as CFData, nil),
              let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let width = properties[kCGImagePropertyPixelWidth] as? Int,
              let height = properties[kCGImagePropertyPixelHeight] as? Int,
              width > 0, height > 0, width <= 8192, height <= 8192,
              width * height <= 16 * 1024 * 1024,
              let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
        else { throw ACPClientError.attachment("Clipboard image is invalid or exceeds image limits") }
        let output = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(output, UTType.png.identifier as CFString, 1, nil)
        else { throw ACPClientError.attachment("Could not import clipboard image") }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination), output.length <= TranscriptMedia.maximumBytes
        else { throw ACPClientError.attachment("Clipboard image exceeds the 10 MiB limit") }
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("KitDesktop/DroppedAttachments/\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let url = directory.appendingPathComponent("Clipboard.png")
        try (output as Data).write(to: url, options: .atomic)
        return url
    }
}
