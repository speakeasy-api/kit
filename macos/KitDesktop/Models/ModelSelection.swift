import Foundation

// Transient selection validation always uses the latest server advertisement.
enum ModelSelection {
    static func option(in options: [ConfigOption], id: String, value: String, disabled: Bool) -> ConfigOption? {
        guard !disabled, let current = options.first(where: { $0.id == id }),
              current.valueType == "select", current.currentValue != value,
              current.choices.contains(where: { $0.value == value }) else { return nil }
        return current
    }
}
