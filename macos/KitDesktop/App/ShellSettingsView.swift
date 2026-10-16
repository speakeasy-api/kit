import SwiftUI

struct ShellSettingsView: View {
    @AppStorage(ShellPreferences.pathKey) private var shellPath = ""
    @AppStorage(ShellPreferences.interactiveKey) private var interactiveStartup =
        ProcessInfo.processInfo.environment["KIT_SHELL_INTERACTIVE"] == "1"

    private var trimmedPath: String {
        shellPath.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    var body: some View {
        Form {
            Section("Command shell") {
                TextField("Shell executable", text: $shellPath, prompt: Text("Automatic"))
                Text("Leave blank to detect the shell from KIT_SHELL, SHELL, or your account's login shell, falling back to /bin/sh. Automatic detection skips unsupported shell families such as csh and tcsh. Enter an absolute executable path, such as /bin/zsh, to override detection.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if !trimmedPath.isEmpty && !ShellPreferences.isExecutablePath(trimmedPath) {
                    Text("Choose an executable file using its absolute path. Shell commands will fail until this path is corrected.")
                        .font(.caption)
                        .foregroundStyle(.red)
                }
                Toggle("Load interactive startup files", isOn: $interactiveStartup)
                Text("Shells always run as login shells. Enable interactive startup for tools configured in files such as .zshrc or .bashrc (for example, mise or nvm). Startup scripts may print output, prompt for input, or have other side effects; commands have no terminal or standard input.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                Text("Changes apply when a conversation's Kit process next starts. Restart the app to apply them to all conversations.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .padding()
        .frame(width: 540)
    }
}
