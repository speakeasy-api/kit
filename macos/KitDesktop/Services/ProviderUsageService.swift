import Darwin
import Foundation

enum ProviderUsageError: LocalizedError {
    case timedOut, outputTooLarge, failed(String)

    var errorDescription: String? {
        switch self {
        case .timedOut: return "Provider usage request timed out. Try refreshing."
        case .outputTooLarge: return "Provider usage output exceeded the size limit."
        case .failed(let message): return message
        }
    }
}

/// Runs the read-only CLI command on a worker, without shell evaluation. The same
/// bundled/override/PATH helper lookup as ACP is used, excluding its login shell.
struct ProviderUsageService {
    var executable: URL? = nil
    var timeout: TimeInterval = 40
    var maximumOutputBytes = 256 * 1024

    func fetch() async throws -> String {
        let worker = Task.detached(priority: .utility) { try run() }
        return try await withTaskCancellationHandler {
            try await worker.value
        } onCancel: {
            worker.cancel()
        }
    }

    private func run() throws -> String {
        try Task.checkCancellation()
        let binary = try executable ?? ACPClient.resolveLaunch(allowLoginShell: false).executable
        var descriptors: [Int32] = [0, 0]
        guard pipe(&descriptors) == 0 else { throw ProviderUsageError.failed("Cannot open usage output pipe.") }
        defer { close(descriptors[0]) }
        var actions: posix_spawn_file_actions_t?
        var attributes: posix_spawnattr_t?
        posix_spawn_file_actions_init(&actions)
        posix_spawnattr_init(&attributes)
        defer {
            posix_spawn_file_actions_destroy(&actions)
            posix_spawnattr_destroy(&attributes)
        }
        posix_spawn_file_actions_adddup2(&actions, descriptors[1], STDOUT_FILENO)
        posix_spawn_file_actions_adddup2(&actions, descriptors[1], STDERR_FILENO)
        posix_spawn_file_actions_addclose(&actions, descriptors[0])
        posix_spawn_file_actions_addclose(&actions, descriptors[1])
        posix_spawn_file_actions_addopen(&actions, STDIN_FILENO, "/dev/null", O_RDONLY, 0)
        posix_spawnattr_setflags(&attributes, Int16(POSIX_SPAWN_SETPGROUP))
        posix_spawnattr_setpgroup(&attributes, 0)
        let arguments: [UnsafeMutablePointer<CChar>?] = [binary.path, "usage"].map {
            (argument: String) in argument.withCString { strdup($0) }
        } + [nil]
        let environment = ProcessInfo.processInfo.environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer {
            arguments.forEach { free($0) }
            environment.forEach { free($0) }
        }
        var pid: pid_t = 0
        let spawned = arguments.withUnsafeBufferPointer { argv in
            environment.withUnsafeBufferPointer { env in
                posix_spawn(&pid, binary.path, &actions, &attributes, argv.baseAddress!, env.baseAddress!)
            }
        }
        close(descriptors[1])
        guard spawned == 0 else { throw ProviderUsageError.failed("Could not start the Kit usage helper (\(spawned)).") }
        var reaped = false
        defer {
            // Kill the group even after leader exit: descendants can retain output.
            kill(-pid, SIGKILL)
            if !reaped {
                var status: Int32 = 0
                while waitpid(pid, &status, 0) < 0 && errno == EINTR {}
            }
        }
        _ = fcntl(descriptors[0], F_SETFL, O_NONBLOCK)
        let deadline = ProcessInfo.processInfo.systemUptime + timeout
        var output = Data()
        var buffer = [UInt8](repeating: 0, count: 8192)
        var eof = false
        var status: Int32 = 0
        while !eof || !reaped {
            try Task.checkCancellation()
            guard ProcessInfo.processInfo.systemUptime < deadline else { throw ProviderUsageError.timedOut }
            let count = read(descriptors[0], &buffer, buffer.count)
            if count > 0 {
                guard output.count + count <= maximumOutputBytes else { throw ProviderUsageError.outputTooLarge }
                output.append(contentsOf: buffer.prefix(count))
            } else if count == 0 { eof = true }
            else if errno != EAGAIN && errno != EINTR { throw ProviderUsageError.failed("Cannot read provider usage.") }
            if !reaped {
                let result = waitpid(pid, &status, WNOHANG)
                if result == pid { reaped = true }
                else if result < 0 && errno != EINTR { throw ProviderUsageError.failed("Cannot wait for provider usage helper.") }
            }
            if count <= 0 && (!eof || !reaped) { usleep(10_000) }
        }
        let text = String(decoding: output, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        guard status == 0 else { throw ProviderUsageError.failed(text.isEmpty ? "Kit usage failed." : text) }
        return text.isEmpty ? "No provider usage is available. Authenticate with Kit to view supported account quotas." : text
    }
}
