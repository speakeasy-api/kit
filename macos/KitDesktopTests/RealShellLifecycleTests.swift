import Foundation
import XCTest
@testable import Kit

/// Only the provider is fake: desktop spawn, ACP v2, compose and ShellTool are real.
final class RealShellLifecycleTests: XCTestCase {
    func testDesktopLaunchExecutesRealComposeShell() throws {
        let repository = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let binary = repository.appendingPathComponent("target/debug/kit")
        guard FileManager.default.isExecutableFile(atPath: binary.path) else {
            throw XCTSkip("Build target/debug/kit before running the real-shell lifecycle regression")
        }
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let config = directory.appendingPathComponent(".kit")
        try FileManager.default.createDirectory(at: config, withIntermediateDirectories: true)
        try Data("credential_store = \"memory\"\n".utf8).write(to: config.appendingPathComponent("config.toml"))

        let server = Process()
        server.executableURL = URL(fileURLWithPath: "/usr/bin/python3")
        server.arguments = [repository.appendingPathComponent("fixtures/mock-openrouter-shell.py").path]
        server.environment = ["HOME": directory.path, "PATH": "/usr/bin:/bin"]
        server.standardInput = FileHandle.nullDevice
        server.standardError = FileHandle.nullDevice
        let portPipe = Pipe()
        server.standardOutput = portPipe
        let listening = expectation(description: "local provider listening")
        var portData = Data()
        var port: Int?
        portPipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            DispatchQueue.main.async {
                guard port == nil else { return }
                portData.append(data)
                if portData.contains(10) {
                    port = Int(String(decoding: portData, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines))
                    listening.fulfill()
                }
            }
        }
        defer {
            portPipe.fileHandleForReading.readabilityHandler = nil
            if server.isRunning { server.terminate(); server.waitUntilExit() }
            try? portPipe.fileHandleForReading.close()
        }
        try server.run()
        wait(for: [listening], timeout: 5)
        let providerPort = try XCTUnwrap(port)

        // env execs the real helper in place; ACPClient still owns the ordinary
        // desktop spawn/process group/pipes. Clear inherited provider/MCP settings.
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/env"),
            prefixArguments: ["-i", "HOME=\(directory.path)", "PATH=/usr/bin:/bin",
                              "OPENROUTER_API_KEY=local-test-key",
                              "OPENROUTER_BASE_URL=http://127.0.0.1:\(providerPort)/stream", binary.path]
        )
        let client = ACPClient(launchOverride: launch, requestTimeout: 15, promptTimeout: 20)
        defer {
            let closed = expectation(description: "real helper closed")
            client.close(activeTurn: true) { closed.fulfill() }
            wait(for: [closed], timeout: 8)
        }
        var updates: [DesktopUpdate] = []
        var finalText = ""
        var receivedFinal = false
        let finished = expectation(description: "real shell final answer")
        client.onUpdate = { update in
            updates.append(update)
            if case .agentMessage(let message) = update {
                let text = message.content.compactMap { block -> String? in
                    if case .text(let value) = block { return value }
                    return nil
                }.joined()
                finalText = message.replace ? text : finalText + text
                if !receivedFinal && (finalText == "REAL_SHELL_COMPLETE" || finalText == "REAL_SHELL_FAILED") {
                    receivedFinal = true
                    finished.fulfill()
                }
            }
        }
        let ready = expectation(description: "real ACP v2 session ready")
        var started = false
        client.start(options: ACPLaunchOptions(root: directory.path, sessionID: UUID().uuidString,
                                               resume: false, provider: "openrouter", model: "test/model",
                                               reasoningEffort: "default"), loading: false) { result in
            switch result {
            case .success: started = true
            case .failure(let error): XCTFail(error.localizedDescription)
            }
            ready.fulfill()
        }
        wait(for: [ready], timeout: 20)
        XCTAssertTrue(started)
        guard started else { return }
        let completed = expectation(description: "real shell prompt completed")
        client.prompt(text: "Run pwd using compose shell", attachments: []) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            completed.fulfill()
        }
        wait(for: [completed, finished], timeout: 25)

        let outputs = updates.compactMap { update -> JSONValue? in
            switch update {
            case .toolCall(let tool), .toolCallUpdate(let tool): return tool.rawOutput
            default: return nil
            }
        }
        let shell = try XCTUnwrap(outputs.first { $0.objectValue?["success"] == .bool(true) }?.objectValue,
                                  "Tool output: \(outputs)")
        XCTAssertEqual(shell["exit_code"], .integer(0))
        XCTAssertEqual(shell["stderr"], .string(""))
        let outputPath = try XCTUnwrap(shell["stdout"]?.stringValue).trimmingCharacters(in: .whitespacesAndNewlines)
        XCTAssertEqual(URL(fileURLWithPath: outputPath).resolvingSymlinksInPath(), directory.resolvingSymlinksInPath())
        let answer = updates.flatMap { update -> [DesktopContentBlock] in
            guard case .agentMessage(let message) = update else { return [] }
            return message.content
        }.compactMap { block -> String? in
            guard case .text(let text) = block else { return nil }
            return text
        }.joined()
        XCTAssertEqual(answer, "REAL_SHELL_COMPLETE")
    }
}
