//  Checks for the .emx grid's model — Grid.swift, with no AppKit in it.
//
//  The window itself is checked by looking at it. These are the parts that
//  cannot be checked that way: whether a typed cell is accepted, what it
//  becomes, and whether what the grid saves is what the grid loaded. Getting
//  any of those wrong writes a wrong number into somebody's table and looks
//  exactly like a right one.
//
//  Run with ../test-grid.sh.

import Foundation

var failures = 0

func check(_ what: String, _ passed: Bool, _ detail: @autoclosure () -> String = "") {
    if passed {
        print("  ok    \(what)")
    } else {
        failures += 1
        let d = detail()
        print("  FAIL  \(what)\(d.isEmpty ? "" : " — \(d)")")
    }
}

func column(_ name: String, _ type: String, unit: String = "", expr: String? = nil) -> GridColumn {
    GridColumn(name: name, type: type, unit: unit, expr: expr)
}

// -- what a column accepts ---------------------------------------------------

print("a column accepts what it declares, and nothing else")

let ints = column("port", "int")
check("an int column takes a whole number", ints.parse("8080") == .whole(8080))
check("an int column refuses text", ints.parse("abc") == nil)
check("an int column refuses a fraction", ints.parse("4.2") == nil,
      "rounding is a decision the file has not made")

let floats = column("distance", "float", unit: "km")
check("a float column takes a decimal", floats.parse("4.2") == .number(4.2))
check("a float column takes a whole number", floats.parse("5") == .number(5))
check("a float column refuses text", floats.parse("abc") == nil)

let flags = column("verified", "bool")
check("a bool column takes true", flags.parse("true") == .flag(true))
check("a bool column takes FALSE in any case", flags.parse("FALSE") == .flag(false))
check("a bool column refuses 1", flags.parse("1") == nil,
      "the file writes true and false, so those are what it reads")

let strings = column("station", "str")
check("a string column keeps leading zeros", strings.parse("007") == .text("007"))
check("a string column keeps the spaces typed into it",
      strings.parse("  north ridge  ") == .text("  north ridge  "),
      "trimming would be the window having an opinion about the data")

// An empty cell is a real value in this format and is not a zero.
for c in [ints, floats, flags, strings] {
    check("an empty \(c.type) cell is empty, not zero", c.parse("") == .empty)
    check("a whitespace-only \(c.type) cell is empty", c.parse("   ") == .empty)
}

// -- how a cell reads back ---------------------------------------------------

print("\nwhat a cell shows is what it parses back to")

let samples: [Cell] = [
    .empty, .whole(0), .whole(-17), .number(4.2), .number(5), .number(0.75),
    .flag(true), .flag(false), .text("007"), .text("north ridge"),
]
for cell in samples {
    let c: GridColumn
    switch cell {
    case .whole: c = ints
    case .number: c = floats
    case .flag: c = flags
    default: c = strings
    }
    // Committing a cell without touching it must not change it. This is the
    // property that a thousands separator or a rounded decimal would break.
    check("\(cell) survives a display/parse round trip",
          c.parse(cell.display) == cell,
          "showed \"\(cell.display)\", parsed back \(String(describing: c.parse(cell.display)))")
}

// -- reading what hearth writes ----------------------------------------------

print("\nthe grid reads what `hearth convert --to json` writes")

let exported = """
{
  "columns": [
    {"name": "station", "type": "str"},
    {"name": "distance", "type": "float", "unit": "m"},
    {"name": "count", "type": "int"},
    {"name": "verified", "type": "bool"},
    {"name": "speed", "type": "float", "unit": "m/s", "expr": "distance / elapsed"}
  ],
  "rows": [
    {"station": "007", "distance": 100.5, "count": 3, "verified": true, "speed": null},
    {"station": "008", "distance": null, "count": 0, "verified": false, "speed": 12.5}
  ]
}
"""

guard let table = TableData.parse(Data(exported.utf8)) else {
    print("  FAIL  could not parse an export at all")
    exit(1)
}

check("every column is read", table.columns.count == 5)
check("every row is read", table.rows.count == 2)
check("the unit travels with the column", table.columns[1].unit == "m")
check("a computed column is marked", table.columns[4].isComputed)
check("a plain column is not", !table.columns[0].isComputed)
check("the header shows the unit", table.columns[1].title == "distance (m)")
check("the header marks the formula", table.columns[4].title.hasSuffix("ƒ"))

// The headline: a column of identifiers stays text because the file said so.
// This is exactly what a CSV round trip has to guess at, and gets wrong.
check("an identifier column stays text", table.rows[0][0] == .text("007"))
check("a declared int is a whole number", table.rows[0][2] == .whole(3))
check("a declared float is a number", table.rows[0][1] == .number(100.5))
// JSONSerialization hands booleans back as NSNumber; mistaking one for a 1
// would put a number in a bool column.
check("true is a boolean, not a 1", table.rows[0][3] == .flag(true))
check("false is a boolean, not a 0", table.rows[1][3] == .flag(false))
check("zero is a zero, not false", table.rows[1][2] == .whole(0))
check("null is an empty cell", table.rows[1][1] == .empty)

// -- writing it back ---------------------------------------------------------

print("\nwhat the grid saves is what the grid loaded")

guard let written = table.json(), let reread = TableData.parse(written) else {
    print("  FAIL  could not write the table back out")
    exit(1)
}
check("the columns survive a save", reread.columns.map(\.name) == table.columns.map(\.name))
check("the types survive a save", reread.columns.map(\.type) == table.columns.map(\.type))
check("the units survive a save", reread.columns.map(\.unit) == table.columns.map(\.unit))
check("the formula survives a save", reread.columns[4].expr == "distance / elapsed")
check("every cell survives a save", reread.rows == table.rows,
      "\(reread.rows) != \(table.rows)")

// A row added in the window is all-empty, and empty is null on the way out —
// not "" and not 0, either of which would be a value nobody entered.
table.rows.append(table.blankRow())
guard let grown = table.json(),
      let obj = try? JSONSerialization.jsonObject(with: grown) as? [String: Any],
      let rows = obj["rows"] as? [[String: Any]]
else {
    print("  FAIL  could not write a table with a new row")
    exit(1)
}
check("a new row is written with every column present", rows[2].count == 5)
check("a new row's cells are null, not empty strings",
      rows[2].values.allSatisfy { $0 is NSNull })

// -- what it refuses to open -------------------------------------------------

print("\nan input the grid cannot show, it declines rather than half-shows")

check("no columns is not a table", TableData.parse(Data(#"{"rows":[]}"#.utf8)) == nil)
check("an empty column list is not a table",
      TableData.parse(Data(#"{"columns":[]}"#.utf8)) == nil)
check("a column with no type is refused",
      TableData.parse(Data(#"{"columns":[{"name":"a"}]}"#.utf8)) == nil,
      "showing its cells under the wrong heading is worse than not showing them")
check("something that is not JSON is refused",
      TableData.parse(Data("not json".utf8)) == nil)
check("a table with no rows is still a table",
      TableData.parse(Data(#"{"columns":[{"name":"a","type":"str"}]}"#.utf8))?.rows.isEmpty == true)

print("")
if failures == 0 {
    print("all grid checks passed")
} else {
    print("\(failures) grid check(s) failed")
    exit(1)
}
