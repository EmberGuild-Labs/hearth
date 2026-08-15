//  Hearth.app — a window for Ember files.
//
//  Opening a file should show you the file and let you change it. The first
//  version of this app rendered a page and handed it to a browser, which is a
//  fine way to look at something and a terrible way to edit it.
//
//  Every operation here shells out to the `hearth` binary carried in this
//  bundle. That is deliberate and load-bearing: the container, the schema,
//  the provenance chain and the round-trip rules live in one implementation,
//  and this window is a view onto it rather than a second opinion about what
//  an Ember file is. Concretely, saving runs
//
//      hearth edit <file> --from <edited>
//
//  which is the same code path as editing from a terminal — so a save from
//  this window keeps the provenance chain, refuses a file with sealed
//  values, and rebuilds the summary tier, without any of that logic being
//  written twice.
//
//  Built with swiftc, not Xcode. See ../build-app.sh.

import AppKit
import UniformTypeIdentifiers

// ---------------------------------------------------------------------------
// Running the hearth binary
// ---------------------------------------------------------------------------

struct Hearth {
    struct Output {
        let status: Int32
        let stdout: String
        let stderr: String
        var ok: Bool { status == 0 }
    }

    /// The binary inside this bundle, falling back to an installed one so the
    /// app still works when it is run from a development build.
    static let binary: URL? = {
        let embedded = Bundle.main.resourceURL?.appendingPathComponent("hearth")
        if let e = embedded, FileManager.default.isExecutableFile(atPath: e.path) { return e }
        for p in ["\(NSHomeDirectory())/.cargo/bin/hearth", "/usr/local/bin/hearth", "/opt/homebrew/bin/hearth"] {
            if FileManager.default.isExecutableFile(atPath: p) { return URL(fileURLWithPath: p) }
        }
        return nil
    }()

    static func run(_ args: [String]) -> Output {
        guard let tool = binary else {
            return Output(status: -1, stdout: "", stderr: "the hearth binary is missing from the application bundle")
        }
        let p = Process()
        p.executableURL = tool
        p.arguments = args
        let out = Pipe(), err = Pipe()
        p.standardOutput = out
        p.standardError = err
        do { try p.run() } catch {
            return Output(status: -1, stdout: "", stderr: "could not run hearth: \(error.localizedDescription)")
        }
        // Read before waiting: a large enough result would fill the pipe and
        // deadlock against a process that never gets to exit.
        let o = out.fileHandleForReading.readDataToEndOfFile()
        let e = err.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return Output(
            status: p.terminationStatus,
            stdout: String(data: o, encoding: .utf8) ?? "",
            stderr: String(data: e, encoding: .utf8) ?? ""
        )
    }
}

// ---------------------------------------------------------------------------
// The table grid (.emx)
// ---------------------------------------------------------------------------
//
//  A `.emx` opened as CSV text is a wall of commas, which is a poor way to
//  read a table and a worse way to edit one. This is the same file as a grid.
//
//  It goes in and out through `hearth convert --to json` and `hearth edit
//  --to json --from`, not CSV, and the reason is types. A CSV round trip
//  re-infers every column from its values, so a column of `007, 008` comes
//  back as the numbers 7 and 8 unless something guesses right. The JSON form
//  carries the declared type, the unit and the formula, so what the grid
//  shows and what it saves are the column the file already declared.

/// The grid itself: an `NSTableView` over a [`TableData`], with every cell
/// checked against its column's declared type before it is accepted.
final class TableGrid: NSObject, NSTableViewDataSource, NSTableViewDelegate {
    let scroll = NSScrollView()
    let table = NSTableView()
    private(set) var data = TableData()

    /// Raised when a cell, or the row count, actually changed.
    var onEdit: () -> Void = {}
    /// Raised with a sentence explaining a cell that was not accepted.
    var onRefusal: (String) -> Void = { _ in }

    override init() {
        super.init()
        table.dataSource = self
        table.delegate = self
        table.usesAlternatingRowBackgroundColors = true
        table.gridStyleMask = [.solidHorizontalGridLineMask, .solidVerticalGridLineMask]
        table.allowsMultipleSelection = true
        table.allowsColumnReordering = false
        table.columnAutoresizingStyle = .noColumnAutoresizing
        table.rowHeight = 20
        table.style = .plain

        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true
        scroll.borderType = .noBorder
        scroll.isHidden = true
    }

    func show(_ d: TableData) {
        data = d
        for c in table.tableColumns { table.removeTableColumn(c) }
        for (i, c) in d.columns.enumerated() {
            let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("c\(i)"))
            col.title = c.title
            col.headerCell.alignment = c.isNumeric ? .right : .left
            // Sized from the header and a sample of the rows rather than the
            // whole column: a million-row scan to pick a width is a million
            // rows read to decide nothing important.
            let widest = d.rows.prefix(200).map { i < $0.count ? $0[i].display.count : 0 }.max() ?? 0
            col.width = min(max(CGFloat(max(c.title.count, widest, 6)) * 7.6 + 18, 60), 320)
            col.minWidth = 44
            table.addTableColumn(col)
        }
        table.reloadData()
    }

    func addRow() {
        data.rows.append(data.blankRow())
        table.reloadData()
        let last = data.rows.count - 1
        table.scrollRowToVisible(last)
        table.selectRowIndexes(IndexSet(integer: last), byExtendingSelection: false)
        onEdit()
    }

    func removeSelectedRows() {
        let selected = table.selectedRowIndexes
        guard !selected.isEmpty else {
            onRefusal("select a row first")
            return
        }
        for i in selected.sorted(by: >) where i < data.rows.count { data.rows.remove(at: i) }
        table.deselectAll(nil)
        table.reloadData()
        onEdit()
    }

    // -- data source ---------------------------------------------------------

    func numberOfRows(in tableView: NSTableView) -> Int { data.rows.count }

    func tableView(_ tv: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard let tableColumn, let i = tv.tableColumns.firstIndex(of: tableColumn),
              i < data.columns.count, row < data.rows.count
        else { return nil }
        let c = data.columns[i]

        let field: NSTextField
        if let reused = tv.makeView(withIdentifier: tableColumn.identifier, owner: self) as? NSTextField {
            field = reused
        } else {
            field = NSTextField()
            field.identifier = tableColumn.identifier
            field.isBordered = false
            field.drawsBackground = false
            field.lineBreakMode = .byTruncatingTail
            // Monospaced digits so a column of numbers lines up, and the
            // substitutions off for the same reason the text editor turns
            // them off: a smart quote in a data cell is corruption, not
            // typography.
            field.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
            field.cell?.usesSingleLineMode = true
            field.target = self
            field.action = #selector(commit(_:))
        }
        field.alignment = c.isNumeric ? .right : .left
        field.isEditable = !c.isComputed
        field.isSelectable = true
        field.textColor = c.isComputed ? .secondaryLabelColor : .labelColor
        field.stringValue = i < data.rows[row].count ? data.rows[row][i].display : ""
        return field
    }

    @objc private func commit(_ sender: NSTextField) {
        let row = table.row(for: sender)
        let col = table.column(for: sender)
        guard row >= 0, row < data.rows.count, col >= 0, col < data.columns.count else { return }
        let c = data.columns[col]
        let was = data.rows[row][col]

        guard let value = c.parse(sender.stringValue) else {
            // The old value goes back in the cell rather than the window
            // keeping something the file would reject. Said in the status
            // line, not an alert: a modal sheet per mistyped cell would make
            // the grid unusable.
            NSSound.beep()
            onRefusal("\(c.name) is \(c.type) — \"\(sender.stringValue)\" is not \(c.expected)")
            sender.stringValue = was.display
            return
        }
        guard value != was else { return }
        data.rows[row][col] = value
        // Re-read from the model, so what is on screen is the value that was
        // stored and not the text that produced it.
        sender.stringValue = value.display
        onEdit()
    }
}

// ---------------------------------------------------------------------------
// One window per file
// ---------------------------------------------------------------------------

final class FileWindow: NSObject, NSWindowDelegate, NSTextViewDelegate {
    let url: URL
    let window: NSWindow

    private let heading = NSTextField(labelWithString: "")
    private let subheading = NSTextField(labelWithString: "")
    private let status = NSTextField(labelWithString: "")
    private let saveButton = NSButton(title: "Save", target: nil, action: nil)
    private let revertButton = NSButton(title: "Revert", target: nil, action: nil)
    private let textView = NSTextView()
    private let imageView = NSImageView()
    private let scroll = NSScrollView()
    private let grid = TableGrid()
    private let addRowButton = NSButton(title: "+ Row", target: nil, action: nil)
    private let deleteRowButton = NSButton(title: "− Row", target: nil, action: nil)

    /// Rows past which the window declines to open a table. A grid holds
    /// every row in memory and a save rewrites the whole file, so there is a
    /// size where the window is simply the wrong tool — and saying so beats
    /// spending a minute finding out.
    private static let gridRowLimit = 50_000

    /// The legacy dialect the payload is edited through — `md` for a `.emt`
    /// that came from Markdown, `csv` for a `.emx`. `hearth edit --export`
    /// decides it and says which it chose.
    private var dialect = ""
    /// Images are shown, not edited: this app has no pixel editor, and
    /// pretending otherwise would mean a Save button that cannot work.
    private var readOnly = false
    private var scratch: URL?

    init(url: URL) {
        self.url = url
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 760, height: 620),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        super.init()

        window.title = url.lastPathComponent
        window.representedURL = url   // gives the title bar the file's own icon
        window.delegate = self
        window.tabbingMode = .disallowed
        // A window built in code defaults to releasing itself when closed,
        // which is a double release once ARC is also holding it: the app
        // segfaults in objc_release when the pool drains, some moments after
        // the window went away. Owning it here and only here is the fix.
        window.isReleasedWhenClosed = false
        build()
        reload()
        window.center()
    }

    // -- layout --------------------------------------------------------------

    private func build() {
        let content = NSView()
        // Painted explicitly rather than left to the window: an unpainted
        // content view is transparent, which reads as white whatever the
        // system appearance is, and puts light text on a light ground.
        content.wantsLayer = true
        content.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        window.contentView = content

        heading.font = .systemFont(ofSize: 13, weight: .semibold)
        subheading.font = .systemFont(ofSize: 11)
        subheading.textColor = .secondaryLabelColor
        status.font = .systemFont(ofSize: 11)
        status.textColor = .secondaryLabelColor
        status.lineBreakMode = .byTruncatingTail

        saveButton.target = self
        saveButton.action = #selector(save)
        saveButton.keyEquivalent = "\r"
        saveButton.isEnabled = false
        revertButton.target = self
        revertButton.action = #selector(revert)
        revertButton.isEnabled = false

        // The substitutions have to go: a smart quote in a JSON file or an
        // em dash in a CSV header is a corrupted document, not a typographic
        // improvement.
        textView.isRichText = false
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        textView.allowsUndo = true
        textView.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        textView.delegate = self
        textView.autoresizingMask = [.width]
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.textContainer?.widthTracksTextView = true
        textView.textContainerInset = NSSize(width: 8, height: 10)

        scroll.documentView = textView
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.borderType = .noBorder
        scroll.drawsBackground = true

        imageView.imageScaling = .scaleProportionallyUpOrDown
        imageView.isHidden = true

        grid.onEdit = { [weak self] in self?.markDirty() }
        grid.onRefusal = { [weak self] why in self?.status.stringValue = why }

        for b in [addRowButton, deleteRowButton] {
            b.target = self
            b.bezelStyle = .rounded
            b.controlSize = .small
            b.font = .systemFont(ofSize: 11)
            b.isHidden = true
        }
        addRowButton.action = #selector(addRow)
        deleteRowButton.action = #selector(deleteRow)

        let views: [NSView] = [
            heading, subheading, status, saveButton, revertButton,
            scroll, imageView, grid.scroll, addRowButton, deleteRowButton,
        ]
        for v in views {
            v.translatesAutoresizingMaskIntoConstraints = false
            content.addSubview(v)
        }

        let pad: CGFloat = 14
        NSLayoutConstraint.activate([
            heading.topAnchor.constraint(equalTo: content.topAnchor, constant: pad),
            heading.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: pad),
            heading.trailingAnchor.constraint(lessThanOrEqualTo: content.trailingAnchor, constant: -pad),

            subheading.topAnchor.constraint(equalTo: heading.bottomAnchor, constant: 2),
            subheading.leadingAnchor.constraint(equalTo: heading.leadingAnchor),
            subheading.trailingAnchor.constraint(lessThanOrEqualTo: content.trailingAnchor, constant: -pad),

            scroll.topAnchor.constraint(equalTo: subheading.bottomAnchor, constant: 10),
            scroll.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: content.trailingAnchor),

            imageView.topAnchor.constraint(equalTo: scroll.topAnchor),
            imageView.leadingAnchor.constraint(equalTo: scroll.leadingAnchor, constant: pad),
            imageView.trailingAnchor.constraint(equalTo: scroll.trailingAnchor, constant: -pad),
            imageView.bottomAnchor.constraint(equalTo: scroll.bottomAnchor, constant: -pad),

            saveButton.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -pad),
            saveButton.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -pad),
            revertButton.centerYAnchor.constraint(equalTo: saveButton.centerYAnchor),
            revertButton.trailingAnchor.constraint(equalTo: saveButton.leadingAnchor, constant: -8),

            // The grid occupies the same slot as the text editor and the
            // image view; exactly one of the three is ever visible.
            grid.scroll.topAnchor.constraint(equalTo: scroll.topAnchor),
            grid.scroll.leadingAnchor.constraint(equalTo: scroll.leadingAnchor),
            grid.scroll.trailingAnchor.constraint(equalTo: scroll.trailingAnchor),
            grid.scroll.bottomAnchor.constraint(equalTo: scroll.bottomAnchor),

            deleteRowButton.centerYAnchor.constraint(equalTo: saveButton.centerYAnchor),
            deleteRowButton.trailingAnchor.constraint(equalTo: revertButton.leadingAnchor, constant: -16),
            addRowButton.centerYAnchor.constraint(equalTo: saveButton.centerYAnchor),
            addRowButton.trailingAnchor.constraint(equalTo: deleteRowButton.leadingAnchor, constant: -6),

            status.centerYAnchor.constraint(equalTo: saveButton.centerYAnchor),
            status.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: pad),
            status.trailingAnchor.constraint(lessThanOrEqualTo: addRowButton.leadingAnchor, constant: -8),

            scroll.bottomAnchor.constraint(equalTo: saveButton.topAnchor, constant: -10),
        ])
    }

    // -- loading -------------------------------------------------------------

    /// Read the file back out of the container. Called on open and after a
    /// save, so what is on screen is always what is in the file rather than
    /// what was typed at it — an importer may normalise, and hiding that
    /// would make the next diff a surprise.
    func reload() {
        let info = Hearth.run(["preview", url.path, "--text"])
        let lines = info.stdout.split(separator: "\n", omittingEmptySubsequences: false)
        heading.stringValue = lines.first.map(String.init) ?? url.lastPathComponent
        subheading.stringValue = lines.count > 1 ? String(lines[1]) : ""
        if !info.ok {
            heading.stringValue = url.lastPathComponent
            subheading.stringValue = "not readable"
            show(problem: info.stderr.isEmpty ? "hearth could not read this file" : info.stderr)
            return
        }

        switch url.pathExtension.lowercased() {
        case "emi":
            loadImage()
            return
        // A table gets a grid. If it cannot have one — the JSON would not
        // parse, or `hearth` could not produce it — fall through to the CSV
        // editor rather than leaving the window with nothing in it.
        case "emx" where loadGrid(rows: rowCount(in: info.stdout)):
            return
        default:
            break
        }

        let tmp = scratchFile(ext: "tmp")
        let export = Hearth.run(["edit", url.path, "--export", tmp.path, "--quiet"])
        guard export.ok else {
            show(problem: export.stderr)
            return
        }
        dialect = export.stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        let text = (try? String(contentsOf: tmp, encoding: .utf8)) ?? ""
        textView.string = text
        textView.undoManager?.removeAllActions()
        readOnly = false
        textView.isEditable = true
        showing(scroll)
        markClean()
        status.stringValue = "editing as .\(dialect)"
    }

    /// The row count out of the summary tier, which `hearth preview` has
    /// already printed by the time this is called.
    ///
    /// Asking costs nothing extra, and the tier exists precisely so that "how
    /// big is this table" can be answered without decoding a row of it.
    private func rowCount(in previewText: String) -> Int? {
        for line in previewText.split(separator: "\n") {
            guard let mark = line.range(of: " rows × ") else { continue }
            return Int(line[line.startIndex..<mark.lowerBound].trimmingCharacters(in: .whitespaces))
        }
        return nil
    }

    /// Show the table as a grid. Returns false when it could not be done and
    /// the caller should fall back to the text editor.
    private func loadGrid(rows: Int?) -> Bool {
        if let n = rows, n > Self.gridRowLimit {
            show(problem: """
                This table has \(n) rows.

                The window loads a table in full and saves it in full, so it \
                does not open one past \(Self.gridRowLimit) rows. The command \
                line reads a table without loading it:

                    hearth view \(url.lastPathComponent) --limit 50
                    hearth edit \(url.lastPathComponent)
                """)
            status.stringValue = "too large to open in a window"
            // Handled, even though nothing was loaded: falling through to the
            // CSV editor would try to put those same rows in a text view.
            return true
        }

        let out = Hearth.run(["convert", url.path, "--to", "json", "-o", "-", "--quiet"])
        guard out.ok,
              let bytes = out.stdout.data(using: .utf8),
              let table = TableData.parse(bytes)
        else { return false }

        grid.show(table)
        // The grid saves through JSON, which is the dialect that keeps the
        // declared types. The text editor still uses CSV.
        dialect = "json"
        readOnly = false
        showing(grid.scroll)
        markClean()
        let n = table.rows.count
        status.stringValue =
            "\(n) row\(n == 1 ? "" : "s") · \(table.columns.count) columns · editing as .json"
        return true
    }

    /// Exactly one of the three content views is visible at a time.
    private func showing(_ view: NSView) {
        for v in [scroll, imageView, grid.scroll] as [NSView] { v.isHidden = v !== view }
        let isGrid = view === grid.scroll
        addRowButton.isHidden = !isGrid
        deleteRowButton.isHidden = !isGrid
    }

    @objc private func addRow() { grid.addRow() }
    @objc private func deleteRow() { grid.removeSelectedRows() }

    private func loadImage() {
        let png = scratchFile(ext: "png")
        try? FileManager.default.removeItem(at: png)
        let out = Hearth.run(["convert", url.path, png.path, "--force", "--quiet"])
        guard out.ok, let image = NSImage(contentsOf: png) else {
            show(problem: out.stderr.isEmpty ? "could not decode this image" : out.stderr)
            return
        }
        imageView.image = image
        showing(imageView)
        readOnly = true
        markClean()
        status.stringValue = "images open here read-only — `hearth edit` opens one in an image editor"
    }

    private func show(problem: String) {
        readOnly = true
        textView.isEditable = false
        textView.string = problem
        showing(scroll)
        markClean()
        status.stringValue = "cannot edit this file"
    }

    // -- saving --------------------------------------------------------------

    @objc func save() {
        guard !readOnly, window.isDocumentEdited else { return }

        // Whichever view holds the edit writes the same dialect it was loaded
        // from, and `hearth edit --from` does the rest. Both routes end in one
        // command, so a save from the grid keeps the provenance chain, refuses
        // a sealed file and rebuilds the summary tier exactly as a save from
        // the text editor does.
        let editing = !grid.scroll.isHidden
        let tmp = scratchFile(ext: editing ? "json" : (dialect.isEmpty ? "txt" : dialect))
        do {
            if editing {
                guard let bytes = grid.data.json() else {
                    alert("Could not save this table", "the grid could not be written as JSON")
                    return
                }
                try bytes.write(to: tmp)
            } else {
                try textView.string.write(to: tmp, atomically: true, encoding: .utf8)
            }
        } catch {
            alert("Could not write a temporary file", error.localizedDescription)
            return
        }

        var args = ["edit", url.path, "--from", tmp.path, "--quiet"]
        // `--to` is needed for the grid because JSON is not the dialect a
        // `.emx` edits in by default; without it the bytes would be read as
        // CSV and rejected.
        if editing { args.insert(contentsOf: ["--to", "json"], at: 2) }
        let out = Hearth.run(args)
        guard out.ok else {
            // The typed text is still in the window, so nothing is lost —
            // say what the file objected to and let it be fixed.
            alert("Hearth could not save this file", out.stderr.trimmingCharacters(in: .whitespacesAndNewlines))
            return
        }

        // `hearth edit` prints the semantic diff of what the save did. The
        // first line names the file; the rest are the changes, and a count of
        // them is the most useful thing a status line can say.
        let lines = out.stdout.split(separator: "\n").map(String.init)
        let changes = max(lines.count - 1, 0)
        reload()
        status.stringValue = changes == 0
            ? "saved — nothing changed"
            : "saved — \(changes) change\(changes == 1 ? "" : "s") recorded in the provenance chain"
    }

    @objc func revert() {
        if window.isDocumentEdited {
            let a = NSAlert()
            a.messageText = "Discard changes to \(url.lastPathComponent)?"
            a.informativeText = "The file on disk has not been changed. What you typed will be lost."
            a.addButton(withTitle: "Discard")
            a.addButton(withTitle: "Cancel")
            a.alertStyle = .warning
            if a.runModal() != .alertFirstButtonReturn { return }
        }
        reload()
    }

    func textDidChange(_ notification: Notification) {
        markDirty()
    }

    private func markDirty() {
        window.isDocumentEdited = true
        saveButton.isEnabled = true
        revertButton.isEnabled = true
        status.stringValue = "edited — not saved"
    }

    private func markClean() {
        window.isDocumentEdited = false
        saveButton.isEnabled = false
        revertButton.isEnabled = !readOnly
    }

    // -- housekeeping --------------------------------------------------------

    private func scratchFile(ext: String) -> URL {
        if scratch == nil {
            let dir = FileManager.default.temporaryDirectory
                .appendingPathComponent("hearth-app-\(ProcessInfo.processInfo.processIdentifier)")
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            scratch = dir
        }
        let stem = url.deletingPathExtension().lastPathComponent
        return scratch!.appendingPathComponent("\(stem).\(ext)")
    }

    private func alert(_ message: String, _ detail: String) {
        let a = NSAlert()
        a.messageText = message
        a.informativeText = detail
        a.alertStyle = .warning
        a.beginSheetModal(for: window)
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        guard window.isDocumentEdited else { return true }
        let a = NSAlert()
        a.messageText = "Save changes to \(url.lastPathComponent)?"
        a.addButton(withTitle: "Save")
        a.addButton(withTitle: "Discard")
        a.addButton(withTitle: "Cancel")
        switch a.runModal() {
        case .alertFirstButtonReturn:
            save()
            return !window.isDocumentEdited
        case .alertSecondButtonReturn:
            return true
        default:
            return false
        }
    }

    func windowWillClose(_ notification: Notification) {
        if let s = scratch { try? FileManager.default.removeItem(at: s) }
        // Dropped on the next turn of the run loop, not here: this is the
        // window's own delegate callback, and releasing the last reference to
        // the delegate while AppKit is still closing the window leaves it
        // messaging an object that has gone.
        DispatchQueue.main.async { AppDelegate.shared?.closed(self) }
    }
}

// ---------------------------------------------------------------------------
// Making a new file
// ---------------------------------------------------------------------------

/// The extra question a save panel has to ask before `hearth create` can
/// answer: which format, and the one thing that format cannot invent for
/// itself.
///
/// A table has no columns and an image has no size, and the command line
/// refuses to guess at either. So does this: the field is where the answer
/// goes, and the format decides what it is called.
final class NewFileOptions: NSObject {
    /// Menu title, extension, field label, placeholder. An empty label means
    /// the format needs nothing beyond a name.
    static let formats: [(String, String, String, String)] = [
        ("Text  (.emt)", "emt", "Title", "optional"),
        ("Document  (.emd)", "emd", "Title", "optional"),
        ("Config  (.emc)", "emc", "", ""),
        ("Table  (.emx)", "emx", "Columns", "station, distance (m), speed (m/s) = distance / elapsed"),
        ("Image  (.emi)", "emi", "Size", "640x480"),
    ]

    let view = NSStackView()
    private let popup = NSPopUpButton()
    private let label = NSTextField(labelWithString: "Title")
    private let field = NSTextField()
    weak var panel: NSSavePanel?

    var chosen: (String, String, String, String) { NewFileOptions.formats[popup.indexOfSelectedItem] }
    var ext: String { chosen.1 }
    var answer: String { field.stringValue.trimmingCharacters(in: .whitespaces) }

    override init() {
        super.init()
        popup.addItems(withTitles: NewFileOptions.formats.map(\.0))
        popup.target = self
        popup.action = #selector(formatChanged)
        field.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        field.controlSize = .small

        view.orientation = .horizontal
        view.spacing = 8
        view.edgeInsets = NSEdgeInsets(top: 10, left: 16, bottom: 10, right: 16)
        view.addArrangedSubview(NSTextField(labelWithString: "Format"))
        view.addArrangedSubview(popup)
        view.addArrangedSubview(label)
        view.addArrangedSubview(field)
        field.widthAnchor.constraint(greaterThanOrEqualToConstant: 240).isActive = true
        formatChanged()
    }

    @objc func formatChanged() {
        let (_, ext, labelText, placeholder) = chosen
        label.stringValue = labelText.isEmpty ? "" : "\(labelText):"
        label.isHidden = labelText.isEmpty
        field.isHidden = labelText.isEmpty
        field.placeholderString = placeholder
        guard let panel else { return }
        if let type = UTType("xyz.ember.\(ext)") { panel.allowedContentTypes = [type] }
        let stem = (panel.nameFieldStringValue as NSString).deletingPathExtension
        panel.nameFieldStringValue = "\(stem.isEmpty ? "Untitled" : stem).\(ext)"
    }

    /// Why a format cannot be created from what has been filled in.
    struct Missing: Error { let reason: String }

    /// The `hearth create` flags this format needs, or the reason it cannot
    /// be created yet.
    func arguments() -> Result<[String], Missing> {
        let (_, ext, labelText, placeholder) = chosen
        if labelText.isEmpty { return .success([]) }
        switch ext {
        case "emx" where answer.isEmpty:
            return .failure(Missing(reason: "A new table needs its columns — for example: \(placeholder)"))
        case "emi" where answer.isEmpty:
            return .failure(Missing(reason: "A new image needs its size in pixels — for example: \(placeholder)"))
        case "emx": return .success(["--columns", answer])
        case "emi": return .success(["--size", answer])
        default: return .success(answer.isEmpty ? [] : ["--title", answer])
        }
    }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

final class AppDelegate: NSObject, NSApplicationDelegate {
    static var shared: AppDelegate?
    private var windows: [FileWindow] = []
    private var openedSomething = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        AppDelegate.shared = self
        buildMenu()

        // The installer launches the app once so macOS marks its exported
        // type declarations trusted — an untrusted declaration's icon is
        // ignored. A file dialog during that step would block it, so the
        // installer leaves a marker and this leaves immediately.
        let marker = URL(fileURLWithPath: NSHomeDirectory())
            .appendingPathComponent("Library/Caches/xyz.ember.hearth.priming")
        if FileManager.default.fileExists(atPath: marker.path) {
            try? FileManager.default.removeItem(at: marker)
            NSApp.terminate(nil)
            return
        }

        // A development hook: render the first window and quit, so the app
        // can be checked without a person clicking anything.
        if let shot = ProcessInfo.processInfo.environment["HEARTH_SNAPSHOT"] {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { self.snapshot(to: shot) }
        }

        sweepScratch()

        // Finder opens documents through openURLs, but a path on the command
        // line has to be handled here — that is how the app is run from a
        // terminal, and how it is tested without a person clicking anything.
        for arg in CommandLine.arguments.dropFirst() where !arg.hasPrefix("-") {
            open(URL(fileURLWithPath: arg))
        }

        // openURLs arrives around launch, so give it a moment before deciding
        // that there is nothing to show.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
            if !self.openedSomething { self.openDocument(nil) }
        }
    }

    func application(_ application: NSApplication, open urls: [URL]) {
        for url in urls { open(url) }
    }

    // Closing the last window leaves the app running, with its menu bar: File
    // > New is a perfectly reasonable thing to want next, and quitting out
    // from under somebody who just closed a document takes that away.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    func open(_ url: URL) {
        openedSomething = true
        if let existing = windows.first(where: { $0.url == url }) {
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let w = FileWindow(url: url)
        windows.append(w)
        w.window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func closed(_ w: FileWindow) {
        windows.removeAll { $0 === w }
    }

    @objc func openDocument(_ sender: Any?) {
        let panel = NSOpenPanel()
        panel.message = "Choose an Ember file"
        panel.allowsMultipleSelection = true
        panel.allowedContentTypes = ["emt", "emd", "emi", "emc", "emx"].compactMap {
            UTType("xyz.ember.\($0)")
        }
        // Cancelling does not quit: the menu bar is still there, and File >
        // New is the other reasonable thing to do at this point.
        if panel.runModal() == .OK {
            for url in panel.urls { open(url) }
        }
    }

    /// Make a new file and open it.
    ///
    /// The file is created by `hearth create`, which builds it through the
    /// ordinary importer — so a file made here is the same file, byte for
    /// byte, as one made from a terminal.
    @objc func newDocument(_ sender: Any?) {
        let panel = NSSavePanel()
        let options = NewFileOptions()
        options.panel = panel
        panel.accessoryView = options.view
        panel.message = "Create a new Ember file"
        panel.nameFieldStringValue = "Untitled.emt"
        panel.canCreateDirectories = true
        panel.isExtensionHidden = false
        if let type = UTType("xyz.ember.emt") { panel.allowedContentTypes = [type] }

        guard panel.runModal() == .OK, let url = panel.url else { return }
        let extra: [String]
        switch options.arguments() {
        case .success(let args): extra = args
        case .failure(let why):
            AppDelegate.alert("Nothing was created", why.reason)
            newDocument(sender)   // ask again rather than making them start over
            return
        }

        // --force because the save panel has already asked about replacing.
        let out = Hearth.run(["create", url.path, "--force", "--quiet"] + extra)
        guard out.ok else {
            AppDelegate.alert(
                "Hearth could not create that file",
                out.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            )
            return
        }
        open(url)
    }

    static func alert(_ message: String, _ detail: String) {
        let a = NSAlert()
        a.messageText = message
        a.informativeText = detail
        a.alertStyle = .warning
        a.runModal()
    }

    @objc func saveDocument(_ sender: Any?) {
        windows.first { $0.window.isKeyWindow }?.save()
    }

    @objc func revertDocument(_ sender: Any?) {
        windows.first { $0.window.isKeyWindow }?.revert()
    }

    /// Delete scratch directories left by earlier runs that are no longer
    /// running.
    ///
    /// A window removes its own on close, but a crash or a kill skips that,
    /// and what is left behind is a plaintext copy of somebody's file —
    /// which for a config is the half of the file they took care to seal.
    /// Named by process id so ownership is decidable rather than guessed at.
    private func sweepScratch() {
        let tmp = FileManager.default.temporaryDirectory
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: tmp, includingPropertiesForKeys: nil) else { return }
        for dir in entries where dir.lastPathComponent.hasPrefix("hearth-app-") {
            let pid = Int32(dir.lastPathComponent.dropFirst("hearth-app-".count)) ?? -1
            if pid == ProcessInfo.processInfo.processIdentifier { continue }
            // kill(pid, 0) asks whether the process exists without signalling it.
            if pid > 0 && kill(pid, 0) == 0 { continue }
            try? FileManager.default.removeItem(at: dir)
        }
    }

    private func snapshot(to path: String) {
        guard let window = windows.first?.window,
              let view = window.contentView,
              let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else {
            NSApp.terminate(nil)
            return
        }
        // Draw in the window's own appearance, or dynamic colours resolve
        // against whatever the process happens to be in and the picture lies
        // about what a person would see.
        view.effectiveAppearance.performAsCurrentDrawingAppearance {
            view.cacheDisplay(in: view.bounds, to: rep)
        }
        if let png = rep.representation(using: .png, properties: [:]) {
            try? png.write(to: URL(fileURLWithPath: path))
        }
        NSApp.terminate(nil)
    }

    /// Built by hand because there is no nib. The Edit menu is not optional:
    /// without its key equivalents, copy and paste do not work in a text view.
    private func buildMenu() {
        let main = NSMenu()

        let appItem = NSMenuItem()
        main.addItem(appItem)
        let appMenu = NSMenu()
        appItem.submenu = appMenu
        appMenu.addItem(withTitle: "About Hearth", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Hide Hearth", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        appMenu.addItem(withTitle: "Quit Hearth", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")

        let fileItem = NSMenuItem()
        main.addItem(fileItem)
        let fileMenu = NSMenu(title: "File")
        fileItem.submenu = fileMenu
        fileMenu.addItem(withTitle: "New…", action: #selector(newDocument(_:)), keyEquivalent: "n").target = self
        fileMenu.addItem(withTitle: "Open…", action: #selector(openDocument(_:)), keyEquivalent: "o").target = self
        fileMenu.addItem(withTitle: "Save", action: #selector(saveDocument(_:)), keyEquivalent: "s").target = self
        fileMenu.addItem(withTitle: "Revert", action: #selector(revertDocument(_:)), keyEquivalent: "r").target = self
        fileMenu.addItem(.separator())
        fileMenu.addItem(withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")

        let editItem = NSMenuItem()
        main.addItem(editItem)
        let editMenu = NSMenu(title: "Edit")
        editItem.submenu = editMenu
        editMenu.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        let redo = editMenu.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        editMenu.addItem(.separator())
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")

        NSApp.mainMenu = main
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
