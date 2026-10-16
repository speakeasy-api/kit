import Foundation

/// Additive UserDefaults keys: absent preferences preserve the launch environment.
enum ShellPreferences {
    static let pathKey = "shellExecutablePath"
    static let interactiveKey = "shellInteractiveStartup"

    static func environment(
        inheriting environment: [String: String], defaults: UserDefaults = .standard
    ) -> [String: String] {
        var result = environment
        let path = (defaults.string(forKey: pathKey) ?? "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if !path.isEmpty { result["KIT_SHELL"] = path }
        if let interactive = defaults.object(forKey: interactiveKey) as? Bool {
            result["KIT_SHELL_INTERACTIVE"] = interactive ? "1" : "0"
        }
        return result
    }

    static func isExecutablePath(_ path: String) -> Bool {
        var isDirectory: ObjCBool = false
        return path.hasPrefix("/")
            && FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory)
            && !isDirectory.boolValue
            && FileManager.default.isExecutableFile(atPath: path)
    }
}
