import Foundation
import XCTest
@testable import Kit

final class ProviderUsageServiceTests: XCTestCase {
    private func fixture(_ body: String) throws -> URL {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appendingPathComponent("kit")
        try Data(("#!/bin/sh\n[ \"$1\" = usage ] || exit 9\n" + body).utf8).write(to: url)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
        return url
    }

    func testReadsUsageCommandOutput() async throws {
        let binary = try fixture("printf 'OpenAI quota: 50%% remaining\\n'\n")
        let text = try await ProviderUsageService(executable: binary).fetch()
        XCTAssertEqual(text, "OpenAI quota: 50% remaining")
    }

    func testReportsNonzeroExit() async throws {
        let binary = try fixture("printf 'Please authenticate' >&2\nexit 2\n")
        do {
            _ = try await ProviderUsageService(executable: binary).fetch()
            XCTFail("Expected error")
        } catch { XCTAssertEqual(error.localizedDescription, "Please authenticate") }
    }

    func testLimitsOutput() async throws {
        let binary = try fixture("while :; do printf '0123456789'; done\n")
        do {
            _ = try await ProviderUsageService(executable: binary, maximumOutputBytes: 64).fetch()
            XCTFail("Expected output limit")
        } catch ProviderUsageError.outputTooLarge {}
    }

    func testTimesOutWhenDescendantRetainsPipe() async throws {
        let binary = try fixture("sleep 60 &\nexit 0\n")
        do {
            _ = try await ProviderUsageService(executable: binary, timeout: 0.1).fetch()
            XCTFail("Expected timeout")
        } catch ProviderUsageError.timedOut {}
    }

    func testCancellationStopsWorker() async throws {
        let binary = try fixture("sleep 60\n")
        let task = Task { try await ProviderUsageService(executable: binary).fetch() }
        try await Task.sleep(nanoseconds: 50_000_000)
        task.cancel()
        do {
            _ = try await task.value
            XCTFail("Expected cancellation")
        } catch is CancellationError {}
    }
}
