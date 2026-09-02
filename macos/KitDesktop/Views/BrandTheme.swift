import AppKit
import SwiftUI

// Tokens mirror speakeasy.com: the primary-colours spectrum, warm paper neutrals,
// mono uppercase micro-labels, and a light serif display face.
enum Brand {
    // Product primary actions are monochrome; blue is reserved for links and focus.
    static let primary = adaptive(light: 0x000000, dark: 0xFFFFFF)
    static let moss = adaptive(light: 0x59824F, dark: 0x76A66A)
    static let ember = adaptive(light: 0xDB7133, dark: 0xFA873C)
    static let vermilion = adaptive(light: 0xC83228, dark: 0xEC6E60)

    static let canvas = adaptive(light: 0xFBFAF8, dark: 0x100E0C)
    static let paper = adaptive(light: 0xF2EFEB, dark: 0x1B1816)
    static let hairline = Color(nsColor: .separatorColor).opacity(0.7)

    static let spectrum = LinearGradient(
        colors: [0x330F1F, 0xC83228, 0xFB873F, 0xD2DC91, 0x5A8250, 0x002314, 0x00143C, 0x2873D7, 0x9BC3FF].map(color),
        startPoint: .leading,
        endPoint: .trailing
    )

    enum Radius {
        static let small: CGFloat = 6
        static let medium: CGFloat = 10
        static let large: CGFloat = 12
    }

    static func display(_ size: CGFloat) -> Font {
        .system(size: size, weight: .light, design: .serif)
    }

    private static func color(_ rgb: Int) -> Color { Color(nsColor: components(rgb)) }

    private static func components(_ rgb: Int) -> NSColor {
        NSColor(
            srgbRed: Double((rgb >> 16) & 0xFF) / 255,
            green: Double((rgb >> 8) & 0xFF) / 255,
            blue: Double(rgb & 0xFF) / 255,
            alpha: 1
        )
    }

    private static func adaptive(light: Int, dark: Int) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? components(dark) : components(light)
        })
    }
}

/// Signature rule from the site header; used once per surface so it stays an accent.
struct BrandSpectrumRule: View {
    var body: some View {
        Brand.spectrum.frame(height: 1.5).accessibilityHidden(true)
    }
}

private struct PointingHandCursorModifier: ViewModifier {
    @Environment(\.isEnabled) private var isEnabled

    func body(content: Content) -> some View {
        content.background {
            PointingHandCursorView(isEnabled: isEnabled)
                .allowsHitTesting(false)
        }
    }
}

private struct PointingHandCursorView: NSViewRepresentable {
    let isEnabled: Bool

    func makeNSView(context: Context) -> CursorView {
        CursorView(isEnabled: isEnabled)
    }

    func updateNSView(_ view: CursorView, context: Context) {
        view.isEnabled = isEnabled
    }

    final class CursorView: NSView {
        var isEnabled: Bool {
            didSet {
                guard isEnabled != oldValue else { return }
                window?.invalidateCursorRects(for: self)
            }
        }

        init(isEnabled: Bool) {
            self.isEnabled = isEnabled
            super.init(frame: .zero)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { nil }

        override func resetCursorRects() {
            super.resetCursorRects()
            if isEnabled { addCursorRect(bounds, cursor: .pointingHand) }
        }

        override func hitTest(_ point: NSPoint) -> NSView? { nil }
    }
}

extension View {
    func pointingHandCursor() -> some View {
        modifier(PointingHandCursorModifier())
    }

    func brandMicroLabel() -> some View {
        font(.system(size: 10, weight: .medium, design: .monospaced))
            .tracking(0.9)
            .textCase(.uppercase)
    }

    /// Mono metadata that keeps its original casing, for values too long to shout.
    func brandMeta() -> some View {
        font(.system(size: 10, weight: .regular, design: .monospaced)).tracking(0.3)
    }

    func brandDisplay(_ size: CGFloat) -> some View {
        font(Brand.display(size)).tracking(-0.4)
    }
}
