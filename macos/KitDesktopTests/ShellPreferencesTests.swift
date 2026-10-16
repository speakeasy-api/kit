import Foundation
import XCTest
@testable import Kit

final class ShellPreferencesTests: XCTestCase {
    private func withDefaults(_ body: (UserDefaults) throws -> Void) throws {
        let suite = "ShellPreferencesTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        try body(defaults)
    }

    func testAbsentPreferencesPreserveInheritedEnvironment() throws {
        try withDefaults { defaults in
            let inherited = ["KIT_SHELL": "/bin/bash", "KIT_SHELL_INTERACTIVE": "1", "PATH": "/bin"]
            XCTAssertEqual(ShellPreferences.environment(inheriting: inherited, defaults: defaults), inherited)
        }
    }

    func testExplicitPreferencesOverrideInheritedValues() throws {
        try withDefaults { defaults in
            defaults.set(" /bin/zsh \n", forKey: ShellPreferences.pathKey)
            defaults.set(false, forKey: ShellPreferences.interactiveKey)
            let result = ShellPreferences.environment(
                inheriting: ["KIT_SHELL": "/bin/bash", "KIT_SHELL_INTERACTIVE": "1", "PATH": "/bin"],
                defaults: defaults
            )
            XCTAssertEqual(result["KIT_SHELL"], "/bin/zsh")
            XCTAssertEqual(result["KIT_SHELL_INTERACTIVE"], "0")
            XCTAssertEqual(result["PATH"], "/bin")
            defaults.set(true, forKey: ShellPreferences.interactiveKey)
            XCTAssertEqual(ShellPreferences.environment(inheriting: [:], defaults: defaults)["KIT_SHELL_INTERACTIVE"], "1")
        }
    }

    func testBlankOverrideRestoresAutomaticSelection() throws {
        try withDefaults { defaults in
            defaults.set("  \n", forKey: ShellPreferences.pathKey)
            XCTAssertNil(ShellPreferences.environment(inheriting: [:], defaults: defaults)["KIT_SHELL"])
        }
    }

    func testMalformedPreferenceTypesDoNotEnableInteractiveStartup() throws {
        try withDefaults { defaults in
            defaults.set(["unexpected"], forKey: ShellPreferences.pathKey)
            defaults.set(["unexpected"], forKey: ShellPreferences.interactiveKey)
            XCTAssertEqual(ShellPreferences.environment(inheriting: [:], defaults: defaults), [:])
        }
    }

    func testInvalidExplicitPathIsNotSilentlyDiscarded() throws {
        try withDefaults { defaults in
            defaults.set("/missing/shell", forKey: ShellPreferences.pathKey)
            XCTAssertEqual(ShellPreferences.environment(inheriting: [:], defaults: defaults)["KIT_SHELL"], "/missing/shell")
        }
    }

    func testExecutableValidation() {
        XCTAssertTrue(ShellPreferences.isExecutablePath("/bin/zsh"))
        XCTAssertFalse(ShellPreferences.isExecutablePath("zsh"))
        XCTAssertFalse(ShellPreferences.isExecutablePath("/bin"))
        XCTAssertFalse(ShellPreferences.isExecutablePath("/missing/shell"))
    }
}
