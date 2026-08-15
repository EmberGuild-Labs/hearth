//  The table model behind the .emx grid in Hearth.app.
//
//  Kept apart from the window on purpose: this file has no AppKit in it, so
//  the rules that decide whether a typed cell is acceptable can be compiled
//  and checked on their own. See ../test-grid.sh.
//
//  Everything here is about the *declared* column. A `.emx` says what each
//  column is; the grid's job is to show that and to refuse anything the file
//  would refuse, before the file is asked.

import Foundation

/// A cell as the file declares it, not as it looks.
enum Cell: Equatable {
    case empty
    case whole(Int)
    case number(Double)
    case flag(Bool)
    case text(String)

    /// What goes in the cell, and — because the cell is editable — exactly
    /// what gets parsed back if it is committed untouched. Nothing is
    /// prettified here for that reason: a thousands separator or a rounded
    /// decimal would make the text differ from the value, and tabbing through
    /// a column would quietly rewrite it.
    var display: String {
        switch self {
        case .empty: return ""
        case .whole(let n): return String(n)
        case .number(let d):
            // Swift's shortest round-trip description, minus the pointless
            // ".0" on a whole-valued float.
            if d.rounded() == d && abs(d) < 1e15 { return String(Int(d)) }
            return String(d)
        case .flag(let b): return b ? "true" : "false"
        case .text(let s): return s
        }
    }
}

/// One column, as `hearth convert --to json` describes it.
struct GridColumn {
    let name: String
    let type: String
    let unit: String
    let expr: String?

    var isComputed: Bool { expr != nil }
    var isNumeric: Bool { type == "int" || type == "float" }

    /// `distance (m)`, or `speed (m/s) ƒ` when the column is computed. The
    /// mark is there because a computed column is not a place to type: its
    /// values come from the formula, and `hearth recompute` overwrites
    /// anything put in by hand.
    var title: String {
        let base = unit.isEmpty ? name : "\(name) (\(unit))"
        return isComputed ? "\(base)  ƒ" : base
    }

    /// What a typed string means in this column, or nil when it means
    /// nothing. The declared type decides, which is the same rule the
    /// importer applies — so a cell this accepts is a cell the file accepts.
    func parse(_ s: String) -> Cell? {
        let trimmed = s.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty { return .empty }
        switch type {
        case "int": return Int(trimmed).map(Cell.whole)
        case "float": return Double(trimmed).map(Cell.number)
        case "bool":
            switch trimmed.lowercased() {
            case "true": return .flag(true)
            case "false": return .flag(false)
            default: return nil
            }
        // Text is taken as typed, spaces included. Trimming a string cell
        // would be this window having an opinion about the user's data.
        default: return .text(s)
        }
    }

    var expected: String {
        switch type {
        case "int": return "a whole number"
        case "float": return "a number"
        case "bool": return "true or false"
        default: return "text"
        }
    }
}

final class TableData {
    var columns: [GridColumn] = []
    var rows: [[Cell]] = []

    /// Read what `hearth convert <file.emx> --to json` writes.
    static func parse(_ data: Data) -> TableData? {
        guard let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let declared = root["columns"] as? [[String: Any]]
        else { return nil }

        let t = TableData()
        t.columns = declared.compactMap { c in
            guard let name = c["name"] as? String, let type = c["type"] as? String else { return nil }
            return GridColumn(
                name: name,
                type: type,
                unit: c["unit"] as? String ?? "",
                expr: c["expr"] as? String
            )
        }
        // A column that would not parse is a column whose cells would be
        // displayed under the wrong heading. Refuse the table instead.
        guard t.columns.count == declared.count, !t.columns.isEmpty else { return nil }

        for r in root["rows"] as? [[String: Any]] ?? [] {
            t.rows.append(t.columns.map { cell(r[$0.name], in: $0) })
        }
        return t
    }

    private static func cell(_ v: Any?, in c: GridColumn) -> Cell {
        switch v {
        case let s as String: return .text(s)
        case let n as NSNumber:
            // JSONSerialization hands booleans back as NSNumber too, and the
            // CFBoolean type id is the only reliable way to tell one from a
            // 0 or a 1.
            if CFGetTypeID(n) == CFBooleanGetTypeID() { return .flag(n.boolValue) }
            return c.type == "int" ? .whole(n.intValue) : .number(n.doubleValue)
        // Null, absent, or something a table cell cannot hold. An empty cell
        // is a real value in this format and is not a zero.
        default: return .empty
        }
    }

    /// The same shape back out, for `hearth edit --to json --from`.
    func json() -> Data? {
        let cols: [[String: Any]] = columns.map {
            var o: [String: Any] = ["name": $0.name, "type": $0.type]
            if !$0.unit.isEmpty { o["unit"] = $0.unit }
            if let e = $0.expr { o["expr"] = e }
            return o
        }
        let rs: [[String: Any]] = rows.map { row in
            var o: [String: Any] = [:]
            for (i, c) in columns.enumerated() where i < row.count {
                switch row[i] {
                case .empty: o[c.name] = NSNull()
                case .whole(let n): o[c.name] = n
                case .number(let d): o[c.name] = d
                case .flag(let b): o[c.name] = b
                case .text(let s): o[c.name] = s
                }
            }
            return o
        }
        return try? JSONSerialization.data(
            withJSONObject: ["columns": cols, "rows": rs],
            options: [.prettyPrinted, .sortedKeys]
        )
    }

    func blankRow() -> [Cell] { columns.map { _ in .empty } }
}

