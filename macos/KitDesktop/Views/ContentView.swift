import SwiftUI
import UniformTypeIdentifiers

struct ContentView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        NavigationSplitView { sidebar } detail: { detail }
            .navigationSplitViewStyle(.balanced)
            .tint(Brand.primary)
            .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
                model.appBecameActive()
            }
            .alert("Persistence Error", isPresented: Binding(
                get: { model.persistenceError != nil },
                set: { if !$0 { model.persistenceError = nil } }
            )) {
                Button("OK") { model.persistenceError = nil }.pointingHandCursor()
            } message: {
                Text(model.persistenceError ?? "")
            }
    }

    private var sidebar: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Text("Kit").brandDisplay(21)
                Spacer()
                Button(action: chooseWorkspace) { Image(systemName: "folder.badge.plus") }
                    .buttonStyle(.plain).pointingHandCursor().help("Add workspace")
                Button(action: model.createConversation) { Image(systemName: "square.and.pencil") }
                    .buttonStyle(.plain).pointingHandCursor()
                    .disabled(model.selectedWorkspaceID == nil).help("New conversation")
            }
            .font(.system(size: 15, weight: .medium))
            .padding(.horizontal, 16).frame(height: 50)
            BrandSpectrumRule()

            if model.state.workspaces.isEmpty {
                Spacer()
                VStack(spacing: 12) {
                    Image(systemName: "folder.badge.plus").font(.system(size: 28)).foregroundStyle(.secondary)
                    Text("Add a workspace").brandDisplay(20)
                    Text("Choose a project folder to start.").font(.callout).foregroundStyle(.secondary)
                    Button("Choose Folder", action: chooseWorkspace).pointingHandCursor()
                }.multilineTextAlignment(.center).padding(24)
                Spacer()
            } else {
                workspacePicker.padding(.horizontal, 12).padding(.top, 10).padding(.bottom, 10)
                Divider().opacity(0.55)
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 3) {
                        HStack {
                            Text("Conversations").brandMicroLabel().foregroundStyle(.tertiary)
                            Spacer()
                        }.padding(.horizontal, 10).padding(.top, 13).padding(.bottom, 5)
                        ForEach(model.workspaceConversations) { conversation in
                            Button { model.selectConversation(conversation.id) } label: {
                                ConversationRow(
                                    conversation: conversation,
                                    selected: conversation.id == model.selectedConversationID,
                                    running: model.activity[conversation.id] == true,
                                    locked: model.lockedConversationIDs.contains(conversation.id)
                                )
                            }.buttonStyle(.plain).pointingHandCursor()
                        }
                        if model.workspaceConversations.isEmpty {
                            VStack(spacing: 8) {
                                Text("No conversations yet").font(.callout).foregroundStyle(.secondary)
                                Button("Start a conversation", action: model.createConversation)
                                    .buttonStyle(.link).pointingHandCursor()
                            }.frame(maxWidth: .infinity).padding(.top, 36)
                        }
                    }.padding(.horizontal, 8).padding(.bottom, 12)
                }
            }
        }
        .background(Brand.paper)
        .navigationSplitViewColumnWidth(min: 240, ideal: 280, max: 340)
    }

    private var workspacePicker: some View {
        Menu {
            ForEach(model.state.workspaces) { workspace in
                Button { model.selectWorkspace(workspace.id) } label: {
                    if workspace.id == model.selectedWorkspaceID { Label(workspace.name, systemImage: "checkmark") }
                    else { Text(workspace.name) }
                }
            }
            Divider()
            Button("Add Workspace…", action: chooseWorkspace)
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "folder").foregroundStyle(.secondary)
                Text(model.selectedWorkspace?.name ?? "Workspace").fontWeight(.medium).lineLimit(1)
                Spacer()
                Image(systemName: "chevron.up.chevron.down").font(.caption2).foregroundStyle(.tertiary)
            }
            .padding(.horizontal, 10).frame(height: 34)
            .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: Brand.Radius.small))
            .overlay { RoundedRectangle(cornerRadius: Brand.Radius.small).stroke(Brand.hairline) }
        }.buttonStyle(.plain)
    }

    @ViewBuilder
    private var detail: some View {
        if let controller = model.selectedController {
            let title = model.state.conversations.first(where: { $0.id == model.selectedConversationID })?.title ?? "Conversation"
            ConversationView(controller: controller, title: title)
                .id(controller.conversationID)
        } else {
            VStack(spacing: 14) {
                Image("KitMark")
                    .resizable()
                    .scaledToFit()
                    .frame(width: 44, height: 44)
                    .accessibilityHidden(true)
                Text(model.selectedWorkspace == nil ? "Add a workspace to begin" : "What should we work on?")
                    .brandDisplay(30)
                if model.selectedWorkspace != nil {
                    Button("New Conversation", action: model.createConversation)
                        .buttonStyle(.borderedProminent).pointingHandCursor()
                }
            }.frame(maxWidth: .infinity, maxHeight: .infinity).background(Brand.canvas)
        }
    }

    private func chooseWorkspace() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Add Workspace"
        if panel.runModal() == .OK, let url = panel.url { model.addWorkspace(path: url.path) }
    }
}

private struct ComposerTextView: NSViewRepresentable {
    @Binding var text: String
    let placeholder: String
    let onDrop: ([URL]) -> Bool
    let onPromisesStarted: (Int) -> Void
    let onPromiseReceived: (Result<URL, Error>) -> Void
    let onTargeted: (Bool) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = ComposerScrollView()
        scrollView.borderType = .noBorder
        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true

        let textView = DroppableTextView()
        textView.delegate = context.coordinator
        textView.isRichText = false
        textView.allowsUndo = true
        textView.drawsBackground = false
        textView.font = NSFont.preferredFont(forTextStyle: .body)
        textView.textColor = .labelColor
        textView.textContainerInset = NSSize(width: 5, height: 7)
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.frame = NSRect(x: 0, y: 0, width: 0, height: 36)
        textView.minSize = NSSize(width: 0, height: 24)
        textView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        textView.textContainer?.containerSize = NSSize(width: 0, height: CGFloat.greatestFiniteMagnitude)
        textView.placeholder = placeholder
        textView.registerForDraggedTypes(
            textView.registeredDraggedTypes + [.fileURL]
                + NSFilePromiseReceiver.readableDraggedTypes.map { NSPasteboard.PasteboardType($0) }
        )
        textView.onDrop = onDrop
        textView.onPromisesStarted = onPromisesStarted
        textView.onPromiseReceived = onPromiseReceived
        textView.onTargeted = onTargeted
        scrollView.documentView = textView
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let textView = scrollView.documentView as? DroppableTextView else { return }
        textView.placeholder = placeholder
        textView.onDrop = onDrop
        textView.onPromisesStarted = onPromisesStarted
        textView.onPromiseReceived = onPromiseReceived
        textView.onTargeted = onTargeted
        if textView.string != text {
            textView.string = text
            textView.needsDisplay = true
        }
        textView.updateDocumentSize()
    }

    func sizeThatFits(
        _ proposal: ProposedViewSize, nsView scrollView: NSScrollView, context: Context
    ) -> CGSize? {
        guard let width = proposal.width, width > 0,
              let textView = scrollView.documentView as? NSTextView
        else { return nil }

        let font = textView.font ?? NSFont.preferredFont(forTextStyle: .body)
        let inset = textView.textContainerInset
        let contentHeight = Self.contentHeight(
            for: text, width: width, font: font, inset: inset,
            lineFragmentPadding: textView.textContainer?.lineFragmentPadding ?? 0
        )
        let maximumHeight = NSLayoutManager().defaultLineHeight(for: font) * 8 + inset.height * 2
        return CGSize(width: width, height: min(max(36, contentHeight), maximumHeight))
    }

    private static func contentHeight(
        for text: String, width: CGFloat, font: NSFont, inset: NSSize, lineFragmentPadding: CGFloat
    ) -> CGFloat {
        let storage = NSTextStorage(string: text, attributes: [.font: font])
        let layoutManager = NSLayoutManager()
        let textContainer = NSTextContainer(
            containerSize: NSSize(
                width: max(0, width - inset.width * 2),
                height: CGFloat.greatestFiniteMagnitude
            )
        )
        textContainer.lineFragmentPadding = lineFragmentPadding
        storage.addLayoutManager(layoutManager)
        layoutManager.addTextContainer(textContainer)
        layoutManager.ensureLayout(for: textContainer)
        let contentBottom = max(
            layoutManager.usedRect(for: textContainer).maxY,
            layoutManager.extraLineFragmentRect.maxY
        )
        return ceil(contentBottom + inset.height * 2)
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: ComposerTextView

        init(_ parent: ComposerTextView) { self.parent = parent }

        func textDidChange(_ notification: Notification) {
            guard let textView = notification.object as? NSTextView else { return }
            parent.text = textView.string
            textView.needsDisplay = true
            (textView as? DroppableTextView)?.updateDocumentSize()
        }
    }
}

private final class ComposerScrollView: NSScrollView {
    override func layout() {
        super.layout()
        (documentView as? DroppableTextView)?.updateDocumentSize()
    }
}

private final class DroppableTextView: NSTextView {
    var placeholder = "" { didSet { needsDisplay = true } }
    var onDrop: (([URL]) -> Bool)?
    var onPromisesStarted: ((Int) -> Void)?
    var onPromiseReceived: ((Result<URL, Error>) -> Void)?
    var onTargeted: ((Bool) -> Void)?
    private var isUpdatingDocumentSize = false
    private static let promiseQueue: OperationQueue = {
        let queue = OperationQueue()
        queue.name = "dev.kit.desktop.file-promises"
        queue.qualityOfService = .userInitiated
        return queue
    }()

    func updateDocumentSize() {
        guard !isUpdatingDocumentSize, frame.width > 0,
              let textContainer, let layoutManager
        else { return }
        isUpdatingDocumentSize = true
        defer { isUpdatingDocumentSize = false }
        layoutManager.ensureLayout(for: textContainer)
        let contentBottom = max(
            layoutManager.usedRect(for: textContainer).maxY,
            layoutManager.extraLineFragmentRect.maxY
        )
        let contentHeight = ceil(contentBottom + textContainerInset.height * 2)
        let viewportHeight = enclosingScrollView?.contentSize.height ?? 36
        let documentHeight = max(contentHeight, viewportHeight)
        guard abs(frame.height - documentHeight) > 0.5 else { return }
        super.setFrameSize(NSSize(width: frame.width, height: documentHeight))
    }

    override func setFrameSize(_ newSize: NSSize) {
        let widthChanged = abs(frame.width - newSize.width) > 0.5
        super.setFrameSize(newSize)
        if widthChanged { updateDocumentSize() }
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard string.isEmpty, !placeholder.isEmpty else { return }
        let padding = textContainer?.lineFragmentPadding ?? 0
        let origin = textContainerOrigin
        let rect = NSRect(
            x: origin.x + padding, y: origin.y,
            width: max(0, bounds.width - origin.x - padding), height: bounds.height - origin.y
        )
        (placeholder as NSString).draw(
            in: rect,
            withAttributes: [
                .font: font ?? NSFont.preferredFont(forTextStyle: .body),
                .foregroundColor: NSColor.placeholderTextColor,
            ]
        )
    }

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        guard canReceiveFiles(from: sender) else { return super.draggingEntered(sender) }
        onTargeted?(true)
        return .copy
    }

    override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation {
        guard canReceiveFiles(from: sender) else { return super.draggingUpdated(sender) }
        return .copy
    }

    override func draggingExited(_ sender: NSDraggingInfo?) {
        onTargeted?(false)
        super.draggingExited(sender)
    }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        defer { onTargeted?(false) }
        let pasteboard = sender.draggingPasteboard
        let promises = pasteboard.readObjects(
            forClasses: [NSFilePromiseReceiver.self], options: nil
        ) as? [NSFilePromiseReceiver] ?? []
        if !promises.isEmpty {
            let promisedFileCount = promises.reduce(0) { $0 + max(1, $1.fileNames.count) }
            onPromisesStarted?(promisedFileCount)
            let receive = onPromiseReceived
            let destination: URL
            do { destination = try makePromiseDestination() }
            catch {
                for _ in 0..<promisedFileCount { receive?(.failure(error)) }
                return true
            }
            for promise in promises {
                promise.receivePromisedFiles(
                    atDestination: destination, options: [:], operationQueue: Self.promiseQueue
                ) { [promise] url, error in
                    _ = promise
                    OperationQueue.main.addOperation {
                        if let error { receive?(.failure(error)) }
                        else { receive?(.success(url)) }
                    }
                }
            }
            return true
        }

        let urls = fileURLs(from: sender)
        guard !urls.isEmpty else { return super.performDragOperation(sender) }
        return onDrop?(urls) ?? false
    }

    private func canReceiveFiles(from sender: NSDraggingInfo) -> Bool {
        !fileURLs(from: sender).isEmpty || sender.draggingPasteboard.canReadObject(
            forClasses: [NSFilePromiseReceiver.self], options: nil
        )
    }

    private func fileURLs(from sender: NSDraggingInfo) -> [URL] {
        let options: [NSPasteboard.ReadingOptionKey: Any] = [.urlReadingFileURLsOnly: true]
        let values = sender.draggingPasteboard.readObjects(forClasses: [NSURL.self], options: options) as? [NSURL] ?? []
        return values.map { $0 as URL }
    }

    private func makePromiseDestination() throws -> URL {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("KitDesktop/DroppedAttachments/\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory
    }
}

private struct ConversationRow: View {
    let conversation: Conversation
    let selected: Bool
    let running: Bool
    let locked: Bool

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            statusMark.padding(.top, 5)
            VStack(alignment: .leading, spacing: 5) {
                Text(conversation.title).font(.callout.weight(conversation.unread ? .semibold : .regular))
                    .foregroundStyle(.primary).lineLimit(2).multilineTextAlignment(.leading)
                HStack(spacing: 6) {
                    Text(conversation.updatedAt, style: .relative)
                    if running { Text("Running").foregroundStyle(Brand.moss) }
                    else if conversation.awaitingUser { Text("Awaiting you").foregroundStyle(Brand.ember) }
                }.font(.caption2).foregroundStyle(.tertiary)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 10).padding(.vertical, 9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(selected ? Brand.primary.opacity(0.1) : .clear, in: RoundedRectangle(cornerRadius: Brand.Radius.small))
        .contentShape(Rectangle())
    }

    @ViewBuilder private var statusMark: some View {
        if locked {
            Image(systemName: "lock.fill")
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(.secondary)
                .frame(width: 9, height: 9)
                .help("Thread is open in another process. Click to try to claim it.")
        } else if running { ProgressView().controlSize(.mini).frame(width: 9, height: 9) }
        else if conversation.awaitingUser { Circle().fill(Brand.ember).frame(width: 7, height: 7) }
        else if conversation.unread { Circle().fill(Brand.vermilion).frame(width: 7, height: 7) }
        else { Circle().fill(.clear).frame(width: 7, height: 7) }
    }
}

private struct ConversationContentLayout: Layout {
    let rosterVisible: Bool
    let rosterBesideComposer: Bool

    func sizeThatFits(
        proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) -> CGSize {
        proposal.replacingUnspecifiedDimensions(by: CGSize(width: 800, height: 600))
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) {
        guard subviews.count >= 2 else { return }
        let hasRoster = rosterVisible && subviews.count >= 3
        let rosterWidth = hasRoster ? min(362, max(262, bounds.width * 0.25)) : 0
        if rosterBesideComposer {
            let primaryWidth = max(0, bounds.width - rosterWidth)
            let composerHeight = min(
                bounds.height,
                subviews[1].sizeThatFits(ProposedViewSize(width: primaryWidth, height: nil)).height
            )
            let transcriptHeight = max(0, bounds.height - composerHeight)
            place(subviews[0], at: bounds.origin, width: primaryWidth, height: transcriptHeight)
            place(
                subviews[1], at: CGPoint(x: bounds.minX, y: bounds.minY + transcriptHeight),
                width: primaryWidth, height: composerHeight
            )
            if hasRoster {
                place(
                    subviews[2], at: CGPoint(x: bounds.minX + primaryWidth, y: bounds.minY),
                    width: rosterWidth, height: bounds.height
                )
            }
        } else {
            let composerHeight = min(
                bounds.height,
                subviews[1].sizeThatFits(ProposedViewSize(width: bounds.width, height: nil)).height
            )
            let transcriptHeight = max(0, bounds.height - composerHeight)
            let transcriptWidth = max(0, bounds.width - rosterWidth)
            place(subviews[0], at: bounds.origin, width: transcriptWidth, height: transcriptHeight)
            if hasRoster {
                place(
                    subviews[2], at: CGPoint(x: bounds.minX + transcriptWidth, y: bounds.minY),
                    width: rosterWidth, height: transcriptHeight
                )
            }
            place(
                subviews[1], at: CGPoint(x: bounds.minX, y: bounds.minY + transcriptHeight),
                width: bounds.width, height: composerHeight
            )
        }
    }

    private func place(_ subview: LayoutSubview, at point: CGPoint, width: CGFloat, height: CGFloat) {
        subview.place(
            at: point, anchor: .topLeading,
            proposal: ProposedViewSize(width: width, height: height)
        )
    }
}

private struct ConversationView: View {
    @ObservedObject var controller: ConversationController
    let title: String
    @State private var choosingFiles = false
    @State private var followTranscript = true
    @State private var showDiagnostics = false
    @State private var showAgentRoster = false
    @State private var isTargetingComposer = false

    var body: some View {
        GeometryReader { geometry in
            VStack(spacing: 0) {
                conversationHeader
                let rosterVisible = showAgentRoster && controller.shouldPresentAgentRoster
                ConversationContentLayout(
                    rosterVisible: rosterVisible,
                    rosterBesideComposer: rosterVisible && geometry.size.width >= 1_200
                ) {
                    transcript
                    composer
                    agentRosterPanel
                }
            }
            .frame(width: geometry.size.width, height: geometry.size.height)
        }
        .background(Brand.canvas)
        .fileImporter(isPresented: $choosingFiles, allowedContentTypes: [.image, .audio], allowsMultipleSelection: true) { result in
            if case .success(let urls) = result { controller.addAttachments(urls) }
        }
    }

    @ViewBuilder private var agentRosterPanel: some View {
        if showAgentRoster && controller.shouldPresentAgentRoster {
            AgentRosterView(roster: controller.agentRoster)
                .frame(minWidth: 240, idealWidth: 290, maxWidth: 340)
                .padding(.leading, 10).padding(.trailing, 12).padding(.vertical, 12)
        }
    }

    private var conversationHeader: some View {
        HStack(spacing: 10) {
            Text(title).font(.headline).lineLimit(1)
            if controller.isRunning {
                HStack(spacing: 5) { ProgressView().controlSize(.mini); Text("Working").brandMicroLabel() }
                    .foregroundStyle(Brand.moss)
            }
            Spacer()
            if controller.shouldPresentAgentRoster {
                Button { showAgentRoster.toggle() } label: {
                    Image(systemName: showAgentRoster ? "person.2.fill" : "person.2")
                }.buttonStyle(.plain).pointingHandCursor()
                    .help(showAgentRoster ? "Hide agent roster" : "Show agent roster")
            }
            Button { followTranscript.toggle() } label: {
                Image(systemName: followTranscript ? "arrow.down.to.line.compact" : "arrow.down")
            }.buttonStyle(.plain).pointingHandCursor()
                .help(followTranscript ? "Following output" : "Resume following")
            Button { controller.copyLastResponse() } label: { Image(systemName: "doc.on.doc") }
                .buttonStyle(.plain).pointingHandCursor().help("Copy last response")
            if !controller.diagnostics.isEmpty {
                Button { showDiagnostics.toggle() } label: { Image(systemName: "exclamationmark.bubble") }
                    .buttonStyle(.plain).pointingHandCursor().help("Diagnostics")
                    .popover(isPresented: $showDiagnostics) { diagnosticsPopover }
            }
        }
        .foregroundStyle(.secondary).padding(.horizontal, 20).frame(height: 50)
        .overlay(alignment: .bottom) { Divider().opacity(0.55) }
    }

    private var diagnosticsPopover: some View {
        ScrollView {
            Text(controller.diagnostics.joined(separator: "\n"))
                .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading).padding()
        }.frame(width: 560, height: 260)
    }

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                if controller.entries.isEmpty {
                    VStack(spacing: 14) {
                        Image("KitMark")
                            .resizable()
                            .scaledToFit()
                            .frame(width: 44, height: 44)
                            .accessibilityHidden(true)
                        Text("What should we work on?").brandDisplay(30)
                    }.frame(maxWidth: .infinity).padding(.top, 150)
                } else {
                    LazyVStack(alignment: .leading, spacing: 22) {
                        ForEach(controller.entries) { entry in
                            TranscriptRow(
                                entry: entry,
                                detach: { if let id = entry.toolCallID { controller.detachCompose(callID: id) } },
                                cancelBackground: { if let id = entry.toolCallID { controller.cancelBackground(callID: id) } }
                            ).id(entry.id)
                        }
                    }
                    .frame(maxWidth: 820, alignment: .leading)
                    .padding(.horizontal, 30).padding(.top, 28).padding(.bottom, 24)
                    .frame(maxWidth: .infinity)
                }
                Color.clear.frame(height: 1).id("bottom")
            }
            .onAppear {
                DispatchQueue.main.async { proxy.scrollTo("bottom", anchor: .bottom) }
            }
            .onChange(of: controller.transcriptRevision) { _, _ in
                if followTranscript { withAnimation(.easeOut(duration: 0.18)) { proxy.scrollTo("bottom", anchor: .bottom) } }
            }
        }
    }

    private var commandSuggestions: [AdvertisedCommand] {
        guard !controller.canCancel, controller.draft.hasPrefix("/") else { return [] }
        let query = String(controller.draft.dropFirst()).lowercased()
        return controller.advertisedCommands.filter { query.isEmpty || $0.name.lowercased().hasPrefix(query) }
    }

    private var composer: some View {
        VStack(spacing: 0) {
            if !controller.pendingSteers.isEmpty { pendingSteersBar }
            if !commandSuggestions.isEmpty { commandBar }
            VStack(spacing: 10) {
                attachmentBar
                ComposerTextView(
                    text: $controller.draft, placeholder: controller.canSteer ? "Steer Kit…" : "Message Kit",
                    onDrop: acceptDroppedFiles,
                    onPromisesStarted: controller.beginReceivingAttachments,
                    onPromiseReceived: controller.finishReceivingAttachment,
                    onTargeted: { isTargetingComposer = $0 }
                )
                HStack(spacing: 10) {
                    Button { choosingFiles = true } label: { Image(systemName: "plus") }
                        .buttonStyle(.plain).font(.system(size: 15, weight: .medium)).pointingHandCursor()
                        .disabled(controller.attachments.count >= ConversationController.maximumAttachmentCount)
                        .help("Attach image or audio")
                    modelControl
                    effortControl
                    contextControl
                    Spacer(minLength: 6)
                    Text(controller.pendingAttachmentReceipts > 0 ? "Receiving attachment…" : controller.status)
                        .brandMeta().foregroundStyle(.tertiary).lineLimit(1)
                    if controller.canCancel {
                        Button(action: controller.cancel) { Image(systemName: "stop.fill") }
                            .buttonStyle(.bordered).controlSize(.small).pointingHandCursor().help("Stop")
                    }
                    Button(action: controller.send) {
                        Image(systemName: "arrow.up").font(.system(size: 13, weight: .bold)).frame(width: 24, height: 24)
                    }
                    .buttonStyle(.borderedProminent)
                    .buttonBorderShape(.roundedRectangle(radius: Brand.Radius.small)).controlSize(.small)
                    .pointingHandCursor().keyboardShortcut(.return, modifiers: .command)
                    .disabled(!canSend)
                }
            }
            .padding(.horizontal, 14).padding(.vertical, 12)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: Brand.Radius.large))
            .overlay {
                RoundedRectangle(cornerRadius: Brand.Radius.large)
                    .stroke(isTargetingComposer ? Brand.primary : Brand.hairline, lineWidth: isTargetingComposer ? 2 : 1)
            }
            .shadow(color: .black.opacity(0.08), radius: 16, y: 5)
            .dropDestination(for: URL.self) { urls, _ in
                acceptDroppedFiles(urls)
            } isTargeted: { isTargetingComposer = $0 }
            .frame(maxWidth: 820)
            .padding(.horizontal, 26).padding(.bottom, 18)
        }.frame(maxWidth: .infinity)
    }

    private var pendingSteersBar: some View {
        VStack(spacing: 5) {
            ForEach(controller.pendingSteers) { item in
                HStack(spacing: 8) {
                    Image(systemName: "clock").foregroundStyle(.secondary)
                    Text(item.summary).lineLimit(1)
                    Spacer()
                    Text("Pending").brandMicroLabel().foregroundStyle(.secondary)
                }
                .font(.caption)
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(Brand.paper, in: RoundedRectangle(cornerRadius: Brand.Radius.small))
            }
        }
        .frame(maxWidth: 790)
        .padding(.bottom, 7)
    }

    private func acceptDroppedFiles(_ urls: [URL]) -> Bool {
        let files = urls.filter(\.isFileURL)
        guard !files.isEmpty else { return false }
        controller.addAttachments(files)
        return true
    }

    private var commandBar: some View {
        HStack(spacing: 14) {
            ForEach(commandSuggestions) { command in
                Button { controller.draft = "/\(command.name) " } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("/\(command.name)").fontWeight(.medium)
                        Text(command.description).foregroundStyle(.secondary)
                    }
                }.buttonStyle(.plain).pointingHandCursor()
            }
            Spacer()
        }
        .font(.caption).padding(10).frame(maxWidth: 790)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: Brand.Radius.medium))
        .overlay { RoundedRectangle(cornerRadius: Brand.Radius.medium).stroke(Brand.hairline) }
        .padding(.bottom, 7)
    }

    @ViewBuilder private var attachmentBar: some View {
        if !controller.attachments.isEmpty {
            ScrollView(.horizontal) {
                HStack(spacing: 7) {
                    ForEach(controller.attachments) { attachment in
                        HStack(spacing: 5) {
                            Image(systemName: attachment.kind == .image ? "photo" : "waveform")
                            Text(attachment.url.lastPathComponent).lineLimit(1)
                            Button { controller.removeAttachment(attachment.id) } label: { Image(systemName: "xmark.circle.fill") }
                                .buttonStyle(.plain).pointingHandCursor()
                        }
                        .font(.caption).padding(.horizontal, 8).padding(.vertical, 5)
                        .background(.quaternary, in: RoundedRectangle(cornerRadius: Brand.Radius.small))
                    }
                }
            }
        }
    }

    @ViewBuilder private var modelControl: some View {
        if let option = controller.configOptions.first(where: { $0.id == "model" }) {
            Menu {
                ForEach(option.groups) { group in
                    Section(group.name) {
                        ForEach(group.choices) { choice in
                            Button { controller.choose(option, value: choice.value) } label: {
                                if choice.value == option.currentValue { Label(choice.name, systemImage: "checkmark") }
                                else { Text(choice.name) }
                            }
                        }
                    }
                }
            } label: {
                HStack(spacing: 4) { Image(systemName: "cpu"); Text(selectedModelLabel(option)).lineLimit(1) }
            }.menuStyle(.borderlessButton).fixedSize().font(.caption)
        }
    }

    @ViewBuilder private var effortControl: some View {
        if let option = reasoningOption, !option.choices.isEmpty {
            HStack(spacing: 6) {
                Image(systemName: "brain.head.profile").foregroundStyle(.secondary)
                if usesSegmentedEffortControl(option) {
                    Picker(option.name, selection: configBinding(option)) {
                        ForEach(option.choices) { choice in
                            Text(choice.name).tag(choice.value)
                        }
                    }
                    .pickerStyle(.segmented).labelsHidden().fixedSize().controlSize(.mini)
                } else {
                    Menu {
                        ForEach(option.groups) { group in
                            Section(group.name) {
                                ForEach(group.choices) { choice in
                                    Button { controller.choose(option, value: choice.value) } label: {
                                        if choice.value == option.currentValue { Label(choice.name, systemImage: "checkmark") }
                                        else { Text(choice.name) }
                                    }
                                }
                            }
                        }
                    } label: { Text(selectedName(option)) }
                    .menuStyle(.borderlessButton).fixedSize().font(.caption)
                }
            }.help(option.name)
        }
    }

    @ViewBuilder private var contextControl: some View {
        if let used = controller.contextUsed, let size = controller.contextSize, size > 0 {
            let percentage = min(100, Int((Double(used) / Double(size) * 100).rounded()))
            HStack(spacing: 5) {
                ProgressView(value: Double(used), total: Double(size)).controlSize(.mini).frame(width: 38)
                Text("\(percentage)%").font(.caption2).monospacedDigit().foregroundStyle(.tertiary)
            }.help("Context: \(used.formatted()) of \(size.formatted()) tokens")
        }
    }

    private var canSend: Bool {
        controller.pendingAttachmentReceipts == 0 && controller.acceptsInput
            && (!controller.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !controller.attachments.isEmpty)
    }

    private func selectedName(_ option: ConfigOption) -> String {
        option.choices.first(where: { $0.value == option.currentValue })?.name ?? option.currentValue.split(separator: ":").last.map(String.init) ?? option.currentValue
    }

    private var reasoningOption: ConfigOption? {
        controller.configOptions.first(where: \.isReasoningEffort)
    }

    private func configBinding(_ option: ConfigOption) -> Binding<String> {
        Binding(
            get: { option.currentValue },
            set: { value in if value != option.currentValue { controller.choose(option, value: value) } }
        )
    }

    private func usesSegmentedEffortControl(_ option: ConfigOption) -> Bool {
        (2...4).contains(option.choices.count) && option.choices.reduce(0) { $0 + $1.name.count } <= 36
    }

    private func selectedModelLabel(_ option: ConfigOption) -> String {
        let pieces = option.currentValue.split(separator: ":", maxSplits: 1).map(String.init)
        guard pieces.count == 2 else { return selectedName(option) }
        return "\(pieces[0]) / \(selectedName(option))"
    }

}

private struct AgentRosterView: View {
    let roster: AgentRoster

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Agents").font(.headline)
                Spacer()
                Text(countSummary).brandMicroLabel().foregroundStyle(.secondary)
            }.padding(.horizontal, 14).frame(height: 42)
            Divider()
            if roster.treeRows.isEmpty {
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: "person.2").foregroundStyle(.secondary)
                    VStack(alignment: .leading, spacing: 3) {
                        Text("No active agents").font(.callout.weight(.medium))
                        Text("Subagents appear here while they work.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                }
                .padding(14)
                Spacer(minLength: 0)
            } else {
                TimelineView(.periodic(from: .now, by: 1)) { timeline in
                    ScrollView {
                        LazyVStack(spacing: 0) {
                            ForEach(roster.treeRows) { treeRow in
                                AgentRosterRowView(treeRow: treeRow, now: timeline.date)
                                Divider().padding(.leading, 14 + CGFloat(treeRow.depth) * 16)
                            }
                        }.padding(.vertical, 6)
                    }
                }
            }
        }
        .background(Brand.paper)
        .clipShape(RoundedRectangle(cornerRadius: Brand.Radius.large))
        .overlay {
            RoundedRectangle(cornerRadius: Brand.Radius.large).stroke(Brand.hairline)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Agent roster")
    }

    private var countSummary: String {
        let counts = roster.counts
        var parts = ["\(counts.total)"]
        if counts.starting > 0 { parts.append("\(counts.starting) starting") }
        if counts.working > 0 { parts.append("\(counts.working) working") }
        if counts.idle > 0 { parts.append("\(counts.idle) idle") }
        return parts.joined(separator: " · ")
    }
}

private struct AgentRosterRowView: View {
    let treeRow: AgentRosterTreeRow
    let now: Date

    private var row: AgentRosterRow { treeRow.row }

    var body: some View {
        HStack(alignment: .top, spacing: 9) {
            Image(systemName: statusSymbol).foregroundStyle(statusColor).frame(width: 12)
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 4) {
                    Text(row.name).font(.callout.weight(.medium)).lineLimit(1)
                    if treeRow.missingParent, let parentName = row.parentName {
                        Text("via \(parentName)").font(.caption2).foregroundStyle(.tertiary).lineLimit(1)
                    }
                    Spacer(minLength: 4)
                    Text(duration).font(.caption2.monospacedDigit()).foregroundStyle(.tertiary)
                }
                Text(row.task).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                Text(statusText).brandMicroLabel().foregroundStyle(statusColor)
            }
        }
        .padding(.leading, 14 + CGFloat(treeRow.depth) * 16).padding(.trailing, 12).padding(.vertical, 9)
        .help("\(row.harness)\(row.model.map { " · \($0)" } ?? "") · generation \(row.generation)")
        .accessibilityElement(children: .combine)
    }

    private var ageMilliseconds: UInt64 {
        let nowMS = UInt64(max(0, now.timeIntervalSince1970 * 1_000))
        let finished = row.generationFinishedAtMS ?? nowMS
        return finished > row.generationStartedAtMS ? finished - row.generationStartedAtMS : 0
    }

    private var duration: String {
        let seconds = ageMilliseconds / 1_000
        if seconds < 60 { return "\(seconds)s" }
        return "\(seconds / 60)m \(seconds % 60)s"
    }

    private var recentFailure: Bool {
        guard row.outcome == .failed, let finished = row.generationFinishedAtMS else { return false }
        let nowMS = UInt64(max(0, now.timeIntervalSince1970 * 1_000))
        return nowMS - min(nowMS, finished) < 4_000
    }

    private var statusText: String {
        if recentFailure { return "Failed" }
        switch row.status {
        case .starting: return "Starting"
        case .working: return "Working"
        case .idle: return "Idle"
        case .removed: return "Removed"
        }
    }

    private var statusSymbol: String {
        if recentFailure { return "xmark.circle.fill" }
        switch row.status {
        case .starting: return "clock"
        case .working: return "circle.dotted"
        case .idle: return "checkmark.circle"
        case .removed: return "circle"
        }
    }

    private var statusColor: Color {
        if recentFailure { return Brand.vermilion }
        switch row.status {
        case .starting: return Brand.ember
        case .working: return Brand.moss
        case .idle, .removed: return .secondary
        }
    }
}

private struct TranscriptRow: View {
    let entry: TranscriptEntry
    let detach: () -> Void
    let cancelBackground: () -> Void

    var body: some View {
        Group {
            switch entry.role {
            case .user: userMessage
            case .assistant: assistantMessage
            case .thought: ThoughtCard(entry: entry)
            case .tool: ToolCard(entry: entry, detach: detach, cancelBackground: cancelBackground)
            case .plan: planMessage
            case .status: statusMessage
            case .error: errorMessage
            case .duration: durationMessage
            case .usage: EmptyView()
            }
        }.frame(maxWidth: .infinity, alignment: entry.role == .user ? .trailing : .leading)
    }

    private var userMessage: some View {
        UserMessageView(entry: entry)
            .padding(.horizontal, 14).padding(.vertical, 10)
            .background(Brand.primary.opacity(0.08), in: RoundedRectangle(cornerRadius: Brand.Radius.large))
            .frame(maxWidth: 570, alignment: .trailing)
    }

    private var assistantMessage: some View {
        MessageText(entry: entry).frame(maxWidth: 760, alignment: .leading)
    }

    private var planMessage: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Plan", systemImage: "checklist").brandMicroLabel().foregroundStyle(.secondary)
            MarkdownView(source: entry.text)
        }
        .padding(14)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: Brand.Radius.medium))
        .overlay { RoundedRectangle(cornerRadius: Brand.Radius.medium).stroke(Brand.hairline) }
    }

    private var statusMessage: some View {
        Label(entry.text, systemImage: "info.circle").brandMeta().foregroundStyle(.tertiary).padding(.vertical, 2)
    }

    private var durationMessage: some View {
        Group {
            if case .turnDuration(let duration) = entry.presentation {
                Label("took \(Self.durationText(duration.milliseconds))", systemImage: "clock")
                    .brandMeta().foregroundStyle(.tertiary)
            }
        }
    }

    private static func durationText(_ milliseconds: Int) -> String {
        if milliseconds < 1_000 { return "\(milliseconds) ms" }
        if milliseconds < 60_000 { return String(format: "%.1f s", Double(milliseconds) / 1_000) }
        return String(format: "%d m %.0f s", milliseconds / 60_000, Double(milliseconds % 60_000) / 1_000)
    }

    private var errorMessage: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Brand.vermilion)
            Text(entry.text).textSelection(.enabled)
        }
        .font(.callout).padding(12)
        .background(Brand.vermilion.opacity(0.08), in: RoundedRectangle(cornerRadius: Brand.Radius.medium))
        .overlay { RoundedRectangle(cornerRadius: Brand.Radius.medium).stroke(Brand.vermilion.opacity(0.25)) }
    }
}

private struct UserMessageView: View {
    let entry: TranscriptEntry

    private var message: UserMessagePresentation {
        entry.presentation?.userMessage ?? UserMessagePresentation(text: entry.text, media: [])
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            if !message.media.isEmpty {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 150), spacing: 8)], alignment: .leading, spacing: 8) {
                    ForEach(message.media) { media in UserMediaView(media: media) }
                }
            }
            if !message.text.isEmpty { MessageText(entry: entry) }
        }
    }
}

private struct UserMediaView: View {
    let media: UserMediaPresentation

    private var image: NSImage? {
        if let data = media.data { return NSImage(data: data) }
        if let url = media.url, url.isFileURL { return NSImage(contentsOf: url) }
        return nil
    }

    var body: some View {
        Group {
            if media.kind == .image, let image {
                Image(nsImage: image).resizable().scaledToFit()
                    .frame(maxWidth: 320, maxHeight: 240)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            } else if media.kind == .image, let url = media.url {
                AsyncImage(url: url) { image in
                    image.resizable().scaledToFit()
                } placeholder: {
                    ProgressView().frame(width: 80, height: 60)
                }
                .frame(maxWidth: 320, maxHeight: 240)
                .clipShape(RoundedRectangle(cornerRadius: 8))
            } else {
                Label(media.name ?? media.mimeType, systemImage: "waveform")
                    .font(.caption).padding(8)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            }
        }
        .accessibilityLabel(media.name ?? (media.kind == .image ? "Image attachment" : "Audio attachment"))
    }
}

private struct MessageText: View {
    let entry: TranscriptEntry
    var body: some View {
        if entry.isStreaming { Text(entry.text).textSelection(.enabled).lineSpacing(3) }
        else { MarkdownView(source: entry.text) }
    }
}

private struct ThoughtCard: View {
    let entry: TranscriptEntry
    @State private var expanded = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if entry.isStreaming {
                HStack(spacing: 9) {
                    ProgressView().controlSize(.small).frame(width: 16, height: 16)
                    Text("Thinking…").font(.callout.weight(.medium))
                    Spacer()
                }
                .padding(.horizontal, 12).padding(.vertical, 10)
                .accessibilityElement(children: .combine)
                .accessibilityLabel("Thinking")
            } else {
                Button { withAnimation(.easeInOut(duration: 0.16)) { expanded.toggle() } } label: {
                    HStack(spacing: 9) {
                        Image(systemName: "brain.head.profile").foregroundStyle(Brand.primary)
                        Text(completedLabel).font(.callout.weight(.medium))
                        Spacer()
                        Image(systemName: expanded ? "chevron.up" : "chevron.down")
                            .font(.caption2).foregroundStyle(.tertiary)
                    }.contentShape(Rectangle())
                }
                .buttonStyle(.plain).pointingHandCursor().padding(.horizontal, 12).padding(.vertical, 10)
            }
            if entry.isStreaming || expanded {
                Divider().opacity(0.55)
                MessageText(entry: entry).foregroundStyle(.secondary).padding(12)
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .animation(.easeInOut(duration: 0.18), value: entry.isStreaming)
        .background(.quaternary.opacity(0.42), in: RoundedRectangle(cornerRadius: Brand.Radius.medium))
        .overlay { RoundedRectangle(cornerRadius: Brand.Radius.medium).stroke(Brand.hairline) }
    }

    private var completedLabel: String {
        guard let milliseconds = entry.presentation?.thought?.milliseconds else { return "Thought" }
        let seconds = max(1, Int((Double(milliseconds) / 1_000).rounded()))
        return "Thought for \(seconds) \(seconds == 1 ? "second" : "seconds")"
    }
}

private struct ComposePresentationView: View {
    private enum Tab: Hashable { case script, output }

    let compose: ComposePresentation
    @State private var selectedTab: Tab

    init(compose: ComposePresentation) {
        self.compose = compose
        _selectedTab = State(initialValue: compose.output == nil ? .script : .output)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Picker("Compose details", selection: $selectedTab) {
                Text("Script").tag(Tab.script)
                Text("Output").tag(Tab.output)
            }
            .pickerStyle(.segmented)
            .labelsHidden()

            switch selectedTab {
            case .script:
                section("Program", text: compose.script)
                if let input = compose.input { section("Input", text: input.formatted) }
                if let background = compose.background {
                    Label(backgroundLabel(background), systemImage: "clock.arrow.circlepath")
                        .font(.caption).foregroundStyle(.secondary)
                }
            case .output:
                if let output = compose.output {
                    section("Output", text: outputText(output))
                } else {
                    Text("No output yet.").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private func outputText(_ output: PresentationJSON) -> String {
        if case .string(let text) = output { return text }
        return output.formatted
    }

    private func backgroundLabel(_ background: ComposeBackgroundRequest) -> String {
        switch background {
        case .immediate(true): "Requested background execution"
        case .immediate(false): "Requested foreground execution"
        case .delay(let seconds): "Background after \(seconds) seconds"
        }
    }

    private func section(_ title: String, text: String) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title).brandMicroLabel().foregroundStyle(.secondary)
            ScrollView([.horizontal, .vertical]) {
                Text(text).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(10)
            }
            .frame(maxHeight: title == "Program" ? 240 : 180)
            .background(Color(nsColor: .textBackgroundColor).opacity(0.6), in: RoundedRectangle(cornerRadius: Brand.Radius.small))
            .overlay { RoundedRectangle(cornerRadius: Brand.Radius.small).stroke(Brand.hairline) }
        }
    }
}

private struct ToolCard: View {
    let entry: TranscriptEntry
    let detach: () -> Void
    let cancelBackground: () -> Void
    @State private var expanded = false

    private var tool: ToolPresentation {
        entry.presentation?.tool ?? ToolPresentation(
            title: entry.title ?? "Tool",
            status: entry.isStreaming ? .inProgress : .unknown,
            detail: entry.text, compose: nil
        )
    }
    private var succeeded: Bool { tool.status == .completed }
    private var failed: Bool { tool.status == .failed }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Button { withAnimation(.easeInOut(duration: 0.16)) { expanded.toggle() } } label: {
                HStack(spacing: 10) {
                    Image(systemName: statusIcon).foregroundStyle(statusColor).frame(width: 17)
                    Text(tool.title).font(.callout.weight(.semibold)).lineLimit(1)
                    Text(tool.status.label)
                        .brandMicroLabel().foregroundStyle(.secondary)
                    Spacer()
                    if !entry.children.isEmpty {
                        let summary = tool.compose != nil && !entry.isStreaming
                            ? ComposePresentation.childToolSummary(entry.children.map(\.tool))
                            : "\(entry.children.count) calls"
                        Text(summary).brandMeta().foregroundStyle(.tertiary).lineLimit(1)
                    }
                    Image(systemName: expanded ? "chevron.up" : "chevron.down").font(.caption2).foregroundStyle(.tertiary)
                }.contentShape(Rectangle())
            }.buttonStyle(.plain).pointingHandCursor().padding(.horizontal, 14).padding(.vertical, 12)

            if expanded {
                Divider().opacity(0.55)
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(entry.children) { child in
                        HStack(spacing: 8) {
                            Image(systemName: childIcon(child))
                                .foregroundStyle(childColor(child))
                            Text(child.tool).font(.caption.weight(.semibold))
                            Text(child.summary).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                            Spacer()
                            if let duration = child.durationMS { Text(durationText(duration)).font(.caption2).foregroundStyle(.tertiary) }
                        }
                    }
                    if let compose = tool.compose {
                        ComposePresentationView(compose: compose)
                    } else if !tool.detail.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                        ScrollView([.horizontal, .vertical]) {
                            Text(tool.detail).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading).padding(10)
                        }
                        .frame(maxHeight: 260)
                        .background(Color(nsColor: .textBackgroundColor).opacity(0.6), in: RoundedRectangle(cornerRadius: Brand.Radius.small))
                        .overlay { RoundedRectangle(cornerRadius: Brand.Radius.small).stroke(Brand.hairline) }
                    }
                    if entry.isStreaming, tool.compose != nil {
                        Button(entry.backgrounded ? "Cancel background call" : "Run in background", action: entry.backgrounded ? cancelBackground : detach)
                            .buttonStyle(.bordered).controlSize(.small).pointingHandCursor()
                    }
                }.padding(12)
            }
        }
        .background(.quaternary.opacity(0.46), in: RoundedRectangle(cornerRadius: Brand.Radius.medium))
        .overlay { RoundedRectangle(cornerRadius: Brand.Radius.medium).stroke(Brand.hairline) }
    }

    private func childIcon(_ child: RuntimeChild) -> String {
        if child.running { return "circle.dotted" }
        return child.succeeded == true ? "checkmark.circle.fill" : "xmark.circle.fill"
    }
    private func childColor(_ child: RuntimeChild) -> Color {
        if child.running { return .secondary }
        return child.succeeded == true ? Brand.moss : Brand.vermilion
    }

    private var statusIcon: String {
        if entry.isStreaming { return "circle.dotted" }
        if succeeded { return "checkmark.circle.fill" }
        if failed { return "xmark.circle.fill" }
        return "wrench.and.screwdriver.fill"
    }
    private var statusColor: Color { entry.isStreaming ? .secondary : (failed ? Brand.vermilion : (succeeded ? Brand.moss : .secondary)) }
    private func durationText(_ milliseconds: Int) -> String {
        milliseconds < 1_000 ? "\(milliseconds) ms" : String(format: "%.1f s", Double(milliseconds) / 1_000)
    }
}
