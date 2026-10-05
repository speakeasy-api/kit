import Foundation
import XCTest
@testable import Kit

final class ProjectDirectoryProtocolTests: XCTestCase {
    @MainActor
    func testDirectoriesAreSentForNewAndResumedSessions() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        for resume in [false, true] {
            let log = directory.appendingPathComponent("\(resume).jsonl")
            let client = makeClient(log: log, supported: true)
            let ready = expectation(description: "directory session ready")
            client.start(options: ACPLaunchOptions(root: directory.path, sessionID: "directory-session", resume: resume,
                         provider: nil, model: nil, reasoningEffort: nil, additionalDirectories: ["/tmp/extra"]), loading: resume) { result in
                if case .failure(let error) = result { XCTFail(error.localizedDescription) }
                ready.fulfill()
            }
            await fulfillment(of: [ready], timeout: 4)
            let closed = expectation(description: "directory session closed")
            client.close(activeTurn: false) { closed.fulfill() }
            await fulfillment(of: [closed], timeout: 4)
            let request = try XCTUnwrap(requests(log).first { $0["method"] as? String == (resume ? "session/resume" : "session/new") })
            XCTAssertEqual(request["additionalDirectories"] as? [String], ["/tmp/extra"])
        }
    }

    @MainActor
    func testUnsupportedDirectoriesFailBeforeSessionCreation() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let log = directory.appendingPathComponent("requests.jsonl")
        let client = makeClient(log: log, supported: false)
        let ready = expectation(description: "unsupported directories rejected")
        client.start(options: ACPLaunchOptions(root: directory.path, sessionID: "unsupported", resume: false,
                     provider: nil, model: nil, reasoningEffort: nil, additionalDirectories: ["/tmp/extra"]), loading: false) { result in
            if case .success = result { XCTFail("Unsupported directories must not be silently ignored") }
            ready.fulfill()
        }
        await fulfillment(of: [ready], timeout: 4)
        let closed = expectation(description: "unsupported helper closed")
        client.close(activeTurn: false) { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
        XCTAssertFalse(try requests(log).contains { $0["method"] as? String == "session/new" })
    }

    @MainActor
    func testControllerUsesUpdatedDirectoriesOnNextStart() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let log = directory.appendingPathComponent("requests.jsonl")
        let controller = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: directory.path,
                         additionalDirectories: ["/tmp/old"], client: makeClient(log: log, supported: true))
        controller.setAdditionalDirectoriesForNextStart(["/tmp/new"])
        let ready = expectation(description: "updated controller ready")
        controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start()
        await fulfillment(of: [ready], timeout: 4)
        let closed = expectation(description: "updated controller closed")
        controller.close { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
        let request = try XCTUnwrap(requests(log).first { $0["method"] as? String == "session/new" })
        XCTAssertEqual(request["additionalDirectories"] as? [String], ["/tmp/new"])
    }

    private func makeClient(log: URL, supported: Bool) -> ACPClient {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let arguments = [root.appendingPathComponent("fixtures/mock-acp-v2.py").path, "--models", "--request-log=" + log.path]
            + (supported ? ["--additional-directories"] : [])
        return ACPClient(launchOverride: ACPClient.LaunchOverride(executable: URL(fileURLWithPath: "/usr/bin/python3"), prefixArguments: arguments), requestTimeout: 2, promptTimeout: 2)
    }

    private func requests(_ url: URL) throws -> [[String: Any]] {
        try String(contentsOf: url).split(separator: "\n").map {
            try XCTUnwrap(JSONSerialization.jsonObject(with: Data($0.utf8)) as? [String: Any])
        }
    }
}
