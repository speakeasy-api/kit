import Darwin
import Foundation

struct ACPLaunchOptions {
    let root: String
    let sessionID: String
    let resume: Bool
    let provider: String?
    let model: String?
    let reasoningEffort: String?
    var force = false
}

struct ACPSessionInfo: Equatable {
    let sessionID: String
    let title: String?
    let updatedAt: Date?
    var isSubagent = false
}

final class ACPClient {
    typealias Dictionary = [String: Any]
    static let protocolVersion = 2
    private static let activeTurnCancelGrace: TimeInterval = 1
    private static let closeRequestTimeout: TimeInterval = 2
    private static let terminationGrace: TimeInterval = 1
    private static let inheritedPipeDrainTimeout: TimeInterval = 2

    struct LaunchOverride {
        let executable: URL
        var prefixArguments: [String] = []
        var environment: [String: String] = [:]
    }

    var onUpdate: ((DesktopUpdate) -> Void)?
    var onRuntimeEvent: ((Dictionary) -> Void)?
    var onDiagnostic: ((String) -> Void)?
    var onExit: ((Int32) -> Void)?

    private struct Pending {
        let method: String
        let completion: (Result<Dictionary, Error>) -> Void
        let timeout: DispatchWorkItem
    }

    private let queue = DispatchQueue(label: "dev.kit.desktop.acp-transport", qos: .userInitiated)
    private let inputPipe = Pipe()
    private let outputPipe = Pipe()
    private let errorPipe = Pipe()
    private let launchOverride: LaunchOverride?
    private let requestTimeout: TimeInterval
    private let promptTimeout: TimeInterval
    private var stdoutParser = JSONLineParser()
    private var stderrParser = JSONLineParser(maximumLineBytes: 1024 * 1024)
    private var nextID = 1
    private var pending: [Int: Pending] = [:]
    private var sessionID: String?
    private var closing = false
    private var exited = false
    private var processPID: pid_t?
    private var processGroup: pid_t?
    private var exitSource: DispatchSourceProcess?
    private var exitStatus: Int32?
    private var stdoutClosed = false
    private var stderrClosed = false
    private var pendingChunk: DesktopUpdate?
    private var closeCompletions: [() -> Void] = []
    private var chunkFlush: DispatchWorkItem?
    private var promptCapabilities: ACPPromptCapabilities?
    private(set) var supportsSteering = false

    init(launchOverride: LaunchOverride? = nil, requestTimeout: TimeInterval = 30, promptTimeout: TimeInterval = 6 * 60 * 60) {
        self.launchOverride = launchOverride
        self.requestTimeout = requestTimeout
        self.promptTimeout = promptTimeout
    }

    func start(options: ACPLaunchOptions, loading: Bool, completion: @escaping (Result<Dictionary, Error>) -> Void) {
        queue.async { self.startLocked(options: options, loading: loading, completion: completion) }
    }

    func listSessions(root: String, completion: @escaping (Result<[ACPSessionInfo], Error>) -> Void) {
        queue.async { self.startListingLocked(root: root, completion: completion) }
    }

    func replacement() -> ACPClient {
        ACPClient(launchOverride: launchOverride, requestTimeout: requestTimeout, promptTimeout: promptTimeout)
    }

    func prompt(text: String, attachments: [Attachment], onSent: (() -> Void)? = nil, completion: @escaping (Result<Dictionary, Error>) -> Void) {
        queue.async {
            guard let sessionID = self.sessionID else {
                self.complete(completion, with: .failure(ACPClientError.protocolError("ACP session is not ready")))
                return
            }
            do {
                let blocks = try self.promptBlocks(text: text, attachments: attachments)
                self.requestLocked(method: "session/prompt", params: ["sessionId": sessionID, "prompt": blocks], timeout: self.promptTimeout, onSent: onSent, completion: completion)
            } catch { self.complete(completion, with: .failure(error)) }
        }
    }

    func inject(
        text: String, attachments: [Attachment],
        completion: @escaping (Result<ACPInjectSessionResponse, Error>) -> Void
    ) {
        queue.async {
            guard let sessionID = self.sessionID else {
                DispatchQueue.main.async { completion(.failure(ACPClientError.protocolError("ACP session is not ready"))) }
                return
            }
            do {
                let blocks = try self.promptBlocks(text: text, attachments: attachments)
                let content = try blocks.map { block in
                    try JSONDecoder().decode(JSONValue.self, from: JSONSerialization.data(withJSONObject: block))
                }
                let request = ACPInjectSessionRequest(sessionId: sessionID, mode: "steer", content: content)
                self.requestLocked(method: "session/inject", params: try Self.dictionary(request)) { result in
                    switch result {
                    case .failure(let error): completion(.failure(error))
                    case .success(let payload):
                        do { completion(.success(try Self.decode(ACPInjectSessionResponse.self, from: payload))) }
                        catch { completion(.failure(ACPClientError.protocolError("Malformed session/inject response: " + error.localizedDescription))) }
                    }
                }
            } catch {
                DispatchQueue.main.async { completion(.failure(error)) }
            }
        }
    }

    func setConfig(id: String, value: ACPSessionConfigValue, completion: @escaping (Result<Dictionary, Error>) -> Void) {
        queue.async {
            guard let sessionID = self.sessionID else { return }
            do {
                let request = ACPSetSessionConfigOptionRequest(sessionId: sessionID, configId: id, value: value)
                self.requestLocked(method: "session/set_config_option", params: try Self.dictionary(request), completion: completion)
            } catch { self.complete(completion, with: .failure(error)) }
        }
    }

    func detachCompose(callID: String, completion: @escaping (Result<Dictionary, Error>) -> Void) {
        privateRequest(method: "kit/compose/detach", callID: callID, completion: completion)
    }

    func cancelBackground(callID: String, completion: @escaping (Result<Dictionary, Error>) -> Void) {
        privateRequest(method: "kit/background/cancel", callID: callID, completion: completion)
    }

    func cancel() {
        queue.async {
            guard let sessionID = self.sessionID else { return }
            self.notifyLocked(method: "session/cancel", params: ["sessionId": sessionID])
        }
    }

    func close(activeTurn: Bool, completion: (() -> Void)? = nil) {
        queue.async {
            if self.exited { if let completion { DispatchQueue.main.async(execute: completion) }; return }
            if let completion { self.closeCompletions.append(completion) }
            if self.processPID == nil {
                self.exited = true
                let completions = self.closeCompletions
                self.closeCompletions.removeAll()
                DispatchQueue.main.async { for completion in completions { completion() } }
                return
            }
            guard !self.closing else { return }
            self.closing = true
            if activeTurn, let sessionID = self.sessionID {
                self.notifyLocked(method: "session/cancel", params: ["sessionId": sessionID])
                self.queue.asyncAfter(deadline: .now() + Self.activeTurnCancelGrace) { self.closeSessionLocked() }
            } else { self.closeSessionLocked() }
        }
    }

    private func startLocked(options: ACPLaunchOptions, loading: Bool, completion: @escaping (Result<Dictionary, Error>) -> Void) {
        do {
            let launch = try launchOverride ?? Self.resolveLaunch()
            try launchLocked(
                executable: launch.executable.path,
                arguments: Self.commandArguments(prefix: launch.prefixArguments, options: options),
                environmentOverrides: launch.environment,
                root: options.root
            )
        } catch {
            exited = true
            complete(completion, with: .failure(error))
            return
        }

        initializeLocked { result in
            switch result {
            case .failure(let error):
                self.close(activeTurn: false)
                self.complete(completion, with: .failure(error))
            case .success:
                let method = loading ? "session/resume" : "session/new"
                if loading { self.sessionID = options.sessionID }
                self.queue.async {
                    do {
                        let params: Dictionary
                        if loading {
                            params = try Self.dictionary(ACPResumeSessionRequest(
                                sessionId: options.sessionID, cwd: options.root, replayFrom: ACPReplayFrom()
                            ))
                        } else {
                            params = try Self.dictionary(ACPNewSessionRequest(cwd: options.root))
                        }
                        self.requestLocked(method: method, params: params) { result in
                            self.queue.async {
                                if case .success(let payload) = result { self.sessionID = payload["sessionId"] as? String ?? options.sessionID }
                                else { self.closeSessionLocked() }
                                self.complete(completion, with: result)
                            }
                        }
                    } catch {
                        self.closeSessionLocked()
                        self.complete(completion, with: .failure(error))
                    }
                }
            }
        }
    }

    private func startListingLocked(root: String, completion: @escaping (Result<[ACPSessionInfo], Error>) -> Void) {
        do {
            let launch = try launchOverride ?? Self.resolveLaunch()
            try launchLocked(
                executable: launch.executable.path,
                arguments: Self.catalogCommandArguments(prefix: launch.prefixArguments, root: root),
                environmentOverrides: launch.environment,
                root: root
            )
        } catch {
            DispatchQueue.main.async { completion(.failure(error)) }
            return
        }
        initializeLocked { result in
            switch result {
            case .failure(let error):
                self.close(activeTurn: false) { completion(.failure(error)) }
            case .success:
                self.queue.async {
                    self.listSessionPageLocked(
                        root: root, cursor: nil, seenCursors: [], sessions: [], completion: completion
                    )
                }
            }
        }
    }

    private func listSessionPageLocked(
        root: String, cursor: String?, seenCursors: Set<String>, sessions: [ACPSessionInfo],
        completion: @escaping (Result<[ACPSessionInfo], Error>) -> Void
    ) {
        let params: Dictionary
        do { params = try Self.dictionary(ACPListSessionsRequest(cwd: root, cursor: cursor)) }
        catch { finishListing(.failure(error), completion: completion); return }
        requestLocked(method: "session/list", params: params) { result in
            self.queue.async {
                switch result {
                case .failure(let error): self.finishListing(.failure(error), completion: completion)
                case .success(let payload):
                    do {
                        let page = try Self.parseSessionPage(payload)
                        var all = sessions
                        var known = Set(all.map(\.sessionID))
                        for session in page.sessions where known.insert(session.sessionID).inserted {
                            all.append(session)
                        }
                        guard let next = page.nextCursor, !next.isEmpty else {
                            self.finishListing(.success(all), completion: completion)
                            return
                        }
                        guard !seenCursors.contains(next) else {
                            throw ACPClientError.protocolError("ACP session/list repeated cursor \(next)")
                        }
                        self.listSessionPageLocked(
                            root: root, cursor: next, seenCursors: seenCursors.union([next]),
                            sessions: all, completion: completion
                        )
                    } catch { self.finishListing(.failure(error), completion: completion) }
                }
            }
        }
    }

    private func finishListing(
        _ result: Result<[ACPSessionInfo], Error>,
        completion: @escaping (Result<[ACPSessionInfo], Error>) -> Void
    ) {
        close(activeTurn: false) { completion(result) }
    }

    static func parseSessionPage(_ payload: Dictionary) throws -> (sessions: [ACPSessionInfo], nextCursor: String?) {
        guard let raw = payload["sessions"] as? [Dictionary] else {
            throw ACPClientError.protocolError("ACP session/list response omitted sessions")
        }
        let sessions = try raw.map { item -> ACPSessionInfo in
            guard let sessionID = item["sessionId"] as? String, !sessionID.isEmpty else {
                throw ACPClientError.protocolError("ACP session/list returned an invalid sessionId")
            }
            let updatedAt = (item["updatedAt"] as? String).flatMap(Self.parseTimestamp)
            let isSubagent = (item["_meta"] as? Dictionary)?["dev.kit.subagent"] as? Bool ?? false
            return ACPSessionInfo(
                sessionID: sessionID, title: item["title"] as? String,
                updatedAt: updatedAt, isSubagent: isSubagent
            )
        }
        return (sessions, payload["nextCursor"] as? String)
    }

    private static func parseTimestamp(_ value: String) -> Date? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.date(from: value) ?? ISO8601DateFormatter().date(from: value)
    }

    private func initializeLocked(completion: @escaping (Result<Dictionary, Error>) -> Void) {
        do {
            let request = ACPInitializeRequest(
                protocolVersion: Self.protocolVersion,
                info: ACPImplementation(name: "kit-desktop", title: "Kit Desktop", version: Self.clientVersion),
                capabilities: ACPClientCapabilities(auth: nil, elicitation: nil)
            )
            requestLocked(method: "initialize", params: try Self.dictionary(request)) { result in
                switch result {
                case .failure: completion(result)
                case .success(let payload):
                    do {
                        let response = try Self.decode(ACPInitializeResponse.self, from: payload)
                        guard response.protocolVersion == Self.protocolVersion else {
                            throw ACPClientError.protocolError("Kit negotiated a non-v2 ACP connection")
                        }
                        self.promptCapabilities = response.capabilities?.session?.prompt
                        let injection = response.capabilities?.session?.inject
                        // Match the TUI: finish-mode steering queues input for the next safe boundary.
                        self.supportsSteering = injection?.modes.contains("steer") == true
                            && injection?.steerInStream?.contains("finish") == true
                        completion(result)
                    } catch {
                        completion(.failure(ACPClientError.protocolError("Malformed ACP v2 initialize response: " + error.localizedDescription)))
                    }
                }
            }
        } catch { completion(.failure(error)) }
    }

    private func launchLocked(
        executable: String, arguments: [String], environmentOverrides: [String: String], root: String
    ) throws {
        var environment = ProcessInfo.processInfo.environment
        environment["KIT_RUNTIME_EVENTS"] = "1"
        for (key, value) in environmentOverrides { environment[key] = value }
        installReaders()
        let pid = try spawn(executable: executable, arguments: arguments, environment: environment, workingDirectory: root)
        processPID = pid
        processGroup = pid
        let source = DispatchSource.makeProcessSource(identifier: pid, eventMask: .exit, queue: queue)
        source.setEventHandler { [weak self] in self?.reapProcessLocked(pid) }
        exitSource = source
        source.resume()
    }

    private static var clientVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "development"
    }

    static func commandArguments(prefix: [String] = [], options: ACPLaunchOptions) -> [String] {
        var arguments = prefix + ["serve", "--stdio-protocol-version", "2", "--root", options.root]
        if let model = options.model { arguments += ["--model", model] }
        if let provider = options.provider { arguments += ["--provider", provider] }
        if let effort = options.reasoningEffort { arguments += ["--reasoning-effort", effort] }
        arguments += ["--session-id", options.sessionID]
        if options.resume { arguments.append("--resume") }
        if options.resume && options.force { arguments.append("--force") }
        return arguments
    }

    static func catalogCommandArguments(prefix: [String] = [], root: String) -> [String] {
        prefix + ["serve", "--stdio-protocol-version", "2", "--root", root]
    }

    private var isProcessRunning: Bool { processPID != nil && exitStatus == nil && !exited }

    private func installReaders() {
        outputPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            if data.isEmpty { handle.readabilityHandler = nil }
            self?.queue.async {
                guard let self else { return }
                if data.isEmpty { self.stdoutEOFLocked() } else { self.consumeStdoutLocked(data) }
            }
        }
        errorPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            if data.isEmpty { handle.readabilityHandler = nil }
            self?.queue.async {
                guard let self else { return }
                if data.isEmpty { self.stderrEOFLocked() } else { self.consumeStderrLocked(data) }
            }
        }
    }

    private func consumeStdoutLocked(_ data: Data) {
        do {
            for line in try stdoutParser.append(data) {
                for message in try RPCEnvelope.parseMany(line) { handleLocked(message) }
            }
        } catch { failTransportLocked(error) }
    }

    private func consumeStderrLocked(_ data: Data) {
        do {
            for line in try stderrParser.append(data) { emitStderrLineLocked(line) }
        } catch { DispatchQueue.main.async { self.onDiagnostic?(error.localizedDescription) } }
    }

    private func emitStderrLineLocked(_ data: Data) {
        guard var line = String(data: data, encoding: .utf8) else { return }
        if line.hasPrefix("\u{1}kit-runtime\u{1}") {
            line.removeFirst(13)
            if let bytes = line.data(using: .utf8), let event = try? JSONSerialization.jsonObject(with: bytes) as? Dictionary {
                DispatchQueue.main.async { self.onRuntimeEvent?(event) }
            }
        } else {
            let capped = String(line.prefix(4096))
            DispatchQueue.main.async { self.onDiagnostic?(capped) }
        }
    }

    private func handleLocked(_ message: RPCEnvelope) {
        if let method = message.method {
            if method == "session/update", let params = message.params, let update = params["update"] as? Dictionary {
                guard let routed = params["sessionId"] as? String, routed == sessionID else {
                    DispatchQueue.main.async { self.onDiagnostic?("Ignored ACP update for another session") }
                    return
                }
                do {
                    let data = try JSONSerialization.data(withJSONObject: update)
                    let wire = try JSONDecoder().decode(JSONValue.self, from: data)
                    let decoded = try DesktopUpdate(wire: wire)
                    if case .unknown(let kind) = decoded {
                        DispatchQueue.main.async { self.onDiagnostic?("Ignored unsupported desktop update: \(kind)") }
                    } else { enqueueUpdateLocked(decoded) }
                } catch {
                    DispatchQueue.main.async { self.onDiagnostic?("Ignored malformed desktop update: \(error.localizedDescription)") }
                }
            } else if method == "session/request_permission", let id = message.id {
                // Desktop has no permission UI yet; cancellation is deterministic and safe.
                do {
                    sendLocked([
                        "jsonrpc": "2.0", "id": id.jsonValue,
                        "result": try Self.dictionary(ACPRequestPermissionResponse()),
                    ])
                } catch { failTransportLocked(error) }
            } else if method == "$/cancel_request" || method == "$/cancelRequest" {
                // A cancellation notification may race the response above.
            } else if method == "kit/turn/state", let params = message.params {
                if let routed = (params["sessionId"] ?? params["session_id"]) as? String, routed != sessionID { return }
                flushChunkLocked()
                let turn = (params["turn_id"] as? NSNumber)?.intValue ?? -1
                emitUpdateLocked(.turnState(DesktopTurnState(turnId: turn, active: params["active"] as? Bool ?? false, error: params["error"] as? String)))
            } else if let id = message.id {
                sendLocked(["jsonrpc": "2.0", "id": id.jsonValue, "error": ["code": -32601, "message": "Client method not supported"]])
            }
            return
        }
        if case .integer(let id) = message.id, let request = pending.removeValue(forKey: id) {
            flushChunkLocked()
            request.timeout.cancel()
            if request.method == "session/new", let routed = message.result?["sessionId"] as? String { sessionID = routed }
            if let error = message.error { complete(request.completion, with: .failure(ACPClientError.remote(error.code, error.message))) }
            else { complete(request.completion, with: .success(message.result ?? [:])) }
        }
    }

    private func enqueueUpdateLocked(_ update: DesktopUpdate) {
        if let key = update.coalescingKey, let text = update.textChunk {
            if let pendingChunk, pendingChunk.coalescingKey == key, let previous = pendingChunk.textChunk {
                self.pendingChunk = pendingChunk.replacingText(previous + text)
            } else {
                flushChunkLocked()
                pendingChunk = update
            }
            if chunkFlush == nil {
                let item = DispatchWorkItem { [weak self] in self?.flushChunkLocked() }
                chunkFlush = item
                queue.asyncAfter(deadline: .now() + 0.033, execute: item)
            }
        } else {
            flushChunkLocked()
            emitUpdateLocked(update)
        }
    }

    private func flushChunkLocked() {
        chunkFlush?.cancel(); chunkFlush = nil
        guard let update = pendingChunk else { return }
        pendingChunk = nil
        emitUpdateLocked(update)
    }

    private func emitUpdateLocked(_ update: DesktopUpdate) {
        let limited = update.limited()
        DispatchQueue.main.async { self.onUpdate?(limited) }
    }

    static func capLargeFields(_ update: Dictionary) -> Dictionary {
        var result = update
        for key in ["rawInput", "rawOutput", "content"] {
            guard let value = result[key], JSONSerialization.isValidJSONObject(value), let data = try? JSONSerialization.data(withJSONObject: value), data.count > 256 * 1024 else { continue }
            if key == "content", update["sessionUpdate"] as? String == "user_message_chunk",
               let content = value as? Dictionary, let type = content["type"] as? String,
               ["image", "audio"].contains(type), data.count <= 14 * 1024 * 1024 {
                continue
            }
            let preview = String(decoding: data.prefix(16 * 1024), as: UTF8.self)
            result[key] = ["truncated": true, "bytes": data.count, "preview": preview]
        }
        return result
    }

    private func privateRequest(method: String, callID: String, completion: @escaping (Result<Dictionary, Error>) -> Void) {
        queue.async {
            guard let sessionID = self.sessionID else { return }
            self.requestLocked(method: method, params: ["session_id": sessionID, "call_id": callID], completion: completion)
        }
    }

    private func requestLocked(method: String, params: Dictionary, timeout: TimeInterval? = nil, onSent: (() -> Void)? = nil, completion: @escaping (Result<Dictionary, Error>) -> Void) {
        guard isProcessRunning, !exited else { complete(completion, with: .failure(ACPClientError.process("Kit process is not running"))); return }
        let id = nextID; nextID += 1
        let work = DispatchWorkItem { [weak self] in
            guard let self, let request = self.pending.removeValue(forKey: id) else { return }
            self.complete(request.completion, with: .failure(ACPClientError.timeout(method)))
        }
        pending[id] = Pending(method: method, completion: completion, timeout: work)
        do { try writeLocked(["jsonrpc": "2.0", "id": id, "method": method, "params": params]) }
        catch {
            pending.removeValue(forKey: id)?.timeout.cancel()
            complete(completion, with: .failure(error))
            return
        }
        if let onSent { DispatchQueue.main.async(execute: onSent) }
        queue.asyncAfter(deadline: .now() + (timeout ?? requestTimeout), execute: work)
    }

    private func notifyLocked(method: String, params: Dictionary) { sendLocked(["jsonrpc": "2.0", "method": method, "params": params]) }

    private func sendLocked(_ object: Dictionary) {
        do { try writeLocked(object) } catch { failTransportLocked(error) }
    }

    private func writeLocked(_ object: Dictionary) throws {
        guard isProcessRunning else { throw ACPClientError.process("Kit process is not running") }
        var data = try JSONSerialization.data(withJSONObject: object)
        data.append(0x0A)
        try inputPipe.fileHandleForWriting.write(contentsOf: data)
    }

    private func closeSessionLocked() {
        guard isProcessRunning else { if let exitStatus { processDidExitLocked(exitStatus) }; return }
        guard let sessionID else { terminateLocked(); return }
        requestLocked(method: "session/close", params: ["sessionId": sessionID], timeout: Self.closeRequestTimeout) { _ in
            self.queue.async { self.terminateLocked() }
        }
        // Request completions are delivered on the main queue. Keep the hard stop on
        // the transport queue so a busy UI cannot push process teardown past app exit.
        queue.asyncAfter(deadline: .now() + Self.closeRequestTimeout) {
            guard self.closing, !self.exited else { return }
            self.terminateLocked()
        }
    }

    private func terminateLocked() {
        try? inputPipe.fileHandleForWriting.close()
        if let group = processGroup { _ = Darwin.kill(-group, SIGTERM) }
        else if let pid = processPID, isProcessRunning { _ = Darwin.kill(pid, SIGTERM) }
        queue.asyncAfter(deadline: .now() + Self.terminationGrace) {
            if let group = self.processGroup {
                if Darwin.kill(-group, 0) == 0 { _ = Darwin.kill(-group, SIGKILL) }
            } else if self.isProcessRunning, let pid = self.processPID { _ = Darwin.kill(pid, SIGKILL) }
        }
    }

    private func stdoutEOFLocked() {
        guard !stdoutClosed else { return }
        stdoutClosed = true
        finishStdoutLocked()
        try? outputPipe.fileHandleForReading.close()
        if let status = exitStatus { finalizeExitLocked(status) }
        else { queue.asyncAfter(deadline: .now() + 1) { if self.stdoutClosed && self.exitStatus == nil { self.failPendingLocked(ACPClientError.process("Kit closed ACP stdout")); self.terminateLocked() } } }
    }

    private func stderrEOFLocked() {
        guard !stderrClosed else { return }
        stderrClosed = true
        do { if let tail = try stderrParser.finish() { emitStderrLineLocked(tail) } }
        catch { DispatchQueue.main.async { self.onDiagnostic?(error.localizedDescription) } }
        try? errorPipe.fileHandleForReading.close()
        if let status = exitStatus { finalizeExitLocked(status) }
    }

    private func finishStdoutLocked() {
        do {
            if let tail = try stdoutParser.finish() { for message in try RPCEnvelope.parseMany(tail) { handleLocked(message) } }
        } catch { failPendingLocked(error) }
    }

    private func reapProcessLocked(_ pid: pid_t) {
        var rawStatus: Int32 = 0
        guard waitpid(pid, &rawStatus, 0) == pid else { return }
        let status = (rawStatus & 0x7f) == 0 ? (rawStatus >> 8) & 0xff : 128 + (rawStatus & 0x7f)
        processDidExitLocked(status)
    }

    private func processDidExitLocked(_ status: Int32) {
        guard exitStatus == nil else { return }
        exitStatus = status
        // The process leader can crash while descendants keep inherited stdio pipes open.
        // Signal the whole group even after the leader has been reaped so those descendants
        // cannot keep shutdown or transport completion alive indefinitely.
        terminateLocked()
        if stdoutClosed && stderrClosed { finalizeExitLocked(status); return }
        queue.asyncAfter(deadline: .now() + Self.inheritedPipeDrainTimeout) {
            guard !self.exited, self.exitStatus != nil else { return }
            self.outputPipe.fileHandleForReading.readabilityHandler = nil
            self.errorPipe.fileHandleForReading.readabilityHandler = nil
            if !self.stdoutClosed { self.stdoutEOFLocked() }
            if !self.stderrClosed { self.stderrEOFLocked() }
        }
    }

    private func finalizeExitLocked(_ status: Int32) {
        guard !exited, stdoutClosed, stderrClosed else { return }
        exited = true
        exitSource?.cancel(); exitSource = nil
        flushChunkLocked()
        failPendingLocked(ACPClientError.process("Kit exited with status \(status)"))
        let completions = closeCompletions; closeCompletions.removeAll()
        DispatchQueue.main.async {
            self.onExit?(status)
            for completion in completions { completion() }
        }
    }

    private func failTransportLocked(_ error: Error) {
        failPendingLocked(error)
        terminateLocked()
    }

    private func failPendingLocked(_ error: Error) {
        let requests = Array(pending.values); pending.removeAll()
        for request in requests { request.timeout.cancel(); complete(request.completion, with: .failure(error)) }
    }

    private func complete(_ completion: @escaping (Result<Dictionary, Error>) -> Void, with result: Result<Dictionary, Error>) {
        DispatchQueue.main.async { completion(result) }
    }

    private func promptBlocks(text: String, attachments: [Attachment]) throws -> [Dictionary] {
        guard attachments.count <= 8 else { throw ACPClientError.attachment("At most 8 attachments can be pending") }
        var total: Int64 = 0
        var blocks: [Dictionary] = []
        let modelText = text
        for attachment in attachments {
            let supported = switch attachment.kind {
            case .image: promptCapabilities?.image != nil
            case .audio: promptCapabilities?.audio != nil
            }
            guard supported else {
                throw ACPClientError.attachment("The connected agent does not support \(attachment.kind.rawValue) prompts")
            }
            let data = try Data(contentsOf: attachment.url, options: .mappedIfSafe)
            guard data.count <= 10 * 1024 * 1024 else { throw ACPClientError.attachment("\(attachment.url.lastPathComponent) exceeds the 10 MiB limit") }
            total += Int64(data.count)
            guard total <= 20 * 1024 * 1024 else { throw ACPClientError.attachment("Attachments exceed the 20 MiB total limit") }
            var block: Dictionary = ["type": attachment.kind.rawValue, "data": data.base64EncodedString(), "mimeType": attachment.mimeType]
            if attachment.kind == .image { block["uri"] = attachment.url.absoluteString }
            blocks.append(block)
        }
        blocks.insert(["type": "text", "text": modelText], at: 0)
        return blocks
    }

    private static func dictionary<T: Encodable>(_ value: T) throws -> Dictionary {
        let data = try JSONEncoder().encode(value)
        guard let object = try JSONSerialization.jsonObject(with: data) as? Dictionary else {
            throw ACPClientError.protocolError("Unable to encode ACP wire object")
        }
        return object
    }

    private static func decode<T: Decodable>(_ type: T.Type, from object: Dictionary) throws -> T {
        try JSONDecoder().decode(type, from: JSONSerialization.data(withJSONObject: object))
    }

    private func spawn(executable: String, arguments: [String], environment: [String: String], workingDirectory: String) throws -> pid_t {
        var actions: posix_spawn_file_actions_t?
        var attributes: posix_spawnattr_t?
        guard posix_spawn_file_actions_init(&actions) == 0, posix_spawnattr_init(&attributes) == 0 else {
            throw ACPClientError.process("Unable to initialize process launcher")
        }
        defer { posix_spawn_file_actions_destroy(&actions); posix_spawnattr_destroy(&attributes) }

        let stdinRead = inputPipe.fileHandleForReading.fileDescriptor
        let stdinWrite = inputPipe.fileHandleForWriting.fileDescriptor
        let stdoutRead = outputPipe.fileHandleForReading.fileDescriptor
        let stdoutWrite = outputPipe.fileHandleForWriting.fileDescriptor
        let stderrRead = errorPipe.fileHandleForReading.fileDescriptor
        let stderrWrite = errorPipe.fileHandleForWriting.fileDescriptor
        let actionResults = [
            posix_spawn_file_actions_adddup2(&actions, stdinRead, STDIN_FILENO),
            posix_spawn_file_actions_adddup2(&actions, stdoutWrite, STDOUT_FILENO),
            posix_spawn_file_actions_adddup2(&actions, stderrWrite, STDERR_FILENO),
            posix_spawn_file_actions_addclose(&actions, stdinRead),
            posix_spawn_file_actions_addclose(&actions, stdinWrite),
            posix_spawn_file_actions_addclose(&actions, stdoutRead),
            posix_spawn_file_actions_addclose(&actions, stdoutWrite),
            posix_spawn_file_actions_addclose(&actions, stderrRead),
            posix_spawn_file_actions_addclose(&actions, stderrWrite),
        ]
        guard actionResults.allSatisfy({ $0 == 0 }) else { throw ACPClientError.process("Unable to configure process pipes") }
        let chdirResult = workingDirectory.withCString { directory in
            if #available(macOS 26, *) { posix_spawn_file_actions_addchdir(&actions, directory) }
            else { posix_spawn_file_actions_addchdir_np(&actions, directory) }
        }
        guard chdirResult == 0,
              posix_spawnattr_setflags(&attributes, Int16(POSIX_SPAWN_SETPGROUP)) == 0,
              posix_spawnattr_setpgroup(&attributes, 0) == 0 else {
            throw ACPClientError.process("Unable to configure process group")
        }

        var argv = ([executable] + arguments).map { strdup($0) } + [nil]
        var envp = environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer { argv.dropLast().forEach { free($0) }; envp.dropLast().forEach { free($0) } }
        var pid: pid_t = 0
        let result = posix_spawn(&pid, executable, &actions, &attributes, &argv, &envp)
        guard result == 0 else { throw ACPClientError.process("Unable to launch Kit: \(String(cString: strerror(result)))") }
        try? inputPipe.fileHandleForReading.close()
        try? outputPipe.fileHandleForWriting.close()
        try? errorPipe.fileHandleForWriting.close()
        return pid
    }

    private static func resolveLaunch() throws -> LaunchOverride {
        let fileManager = FileManager.default
        let bundled = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/kit")
        if fileManager.isExecutableFile(atPath: bundled.path) {
            return LaunchOverride(executable: bundled)
        }

        let environment = ProcessInfo.processInfo.environment
        if let override = environment["KIT_BINARY"], fileManager.isExecutableFile(atPath: override) {
            return LaunchOverride(executable: URL(fileURLWithPath: override))
        }

        #if DEBUG
        let source = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("target/debug/kit")
        if fileManager.isExecutableFile(atPath: source.path) { return LaunchOverride(executable: source) }
        #endif

        if let executable = installedExecutable(environment: environment, fileManager: fileManager) {
            return LaunchOverride(executable: executable)
        }

        // Finder-launched apps receive a minimal PATH. Ask the user's login shell for
        // the same command lookup they get in Terminal, then launch the resolved file
        // directly so every conversation uses an identical executable.
        if let executable = resolveFromLoginShell(environment: environment) {
            return LaunchOverride(executable: executable)
        }
        throw ACPClientError.missingBinary
    }

    static func installedExecutable(
        environment: [String: String], fileManager: FileManager = .default
    ) -> URL? {
        var searchDirectories = environment["PATH"]?.split(separator: ":").map(String.init) ?? []
        if let home = environment["HOME"] { searchDirectories.append(home + "/.cargo/bin") }
        searchDirectories += ["/opt/homebrew/bin", "/usr/local/bin"]
        for directory in searchDirectories {
            let candidate = URL(fileURLWithPath: directory, isDirectory: true).appendingPathComponent("kit")
            if fileManager.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }

    private static func resolveFromLoginShell(environment: [String: String]) -> URL? {
        let shell = environment["SHELL"] ?? "/bin/zsh"
        guard FileManager.default.isExecutableFile(atPath: shell) else { return nil }
        let process = Process()
        let output = Pipe()
        process.executableURL = URL(fileURLWithPath: shell)
        process.arguments = ["-lic", "command -v kit"]
        process.environment = environment
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            process.waitUntilExit()
            guard process.terminationStatus == 0 else { return nil }
            let data = output.fileHandleForReading.readDataToEndOfFile()
            let lines = String(decoding: data, as: UTF8.self).split(whereSeparator: \.isNewline)
            for line in lines.reversed() {
                let path = String(line).trimmingCharacters(in: .whitespacesAndNewlines)
                if path.hasPrefix("/"), FileManager.default.isExecutableFile(atPath: path) {
                    return URL(fileURLWithPath: path)
                }
            }
        } catch {}
        return nil
    }
}
