use super::*;

/// #956: `1.200,00` read as 1.2, so a column of European amounts sorted
/// wrongly. A comma after the last dot, or one comma not followed by
/// exactly three digits, is the decimal separator; several dots group
/// thousands. One separator followed by three digits keeps the English
/// reading (`1,200` is 1200, `1.200` is 1.2): no cell can tell them apart.
#[test]
fn european_numbers_read_as_their_value() {
    let read = [
        ("1.200,00", 1200.0),
        ("€ 1.200,50", 1200.5),
        ("1.200.000", 1_200_000.0),
        ("1.200.000,25", 1_200_000.25),
        ("12,50", 12.5),
        ("12,5 EUR", 12.5),
        ("-3,75", -3.75),
        ("1,200.50", 1200.5),
        ("1,200", 1200.0),
        ("1.200", 1.2),
    ];
    for (text, value) in read {
        assert_eq!(as_number(text), Some(value), "{text}");
    }
    for text in ["1.200,000,00", "5-10", "3 of 10"] {
        assert_eq!(as_number(text), None, "{text}");
    }
}

#[test]
fn a_column_of_european_amounts_sorts_by_value() {
    let rows = [
        row(1, RowKind::Kept, json!({"amount": "99,00"})),
        row(2, RowKind::Kept, json!({"amount": "1.200,00"})),
        row(3, RowKind::Kept, json!({"amount": "150,00"})),
    ];
    let order = OrderBy { column: "amount".into(), desc: true };
    let t = render(&rows, &cols(&["amount"]), Some(&order), &counts(3, 3));
    let cells: Vec<&str> = t.lines().skip(2).take(3).collect();
    assert_eq!(cells, ["| 1.200,00 |", "| 150,00 |", "| 99,00 |"], "{t}");
}

/// #956: a `columns` name that is no field showed an empty column and no
/// word why. Every column but `source` that is empty in every kept row is
/// named, in the table's order; a column any kept row fills (in any case,
/// as the cells read it) is not, and withheld rows (no fields) count for
/// nothing either way.
#[test]
fn a_column_empty_in_every_kept_row_is_named() {
    let rows = [
        row(1, RowKind::Kept, json!({"amount": "10", "vendor": ""})),
        row(2, RowKind::Kept, json!({"Date": "2026-01-02", "vendor": null})),
        row(3, RowKind::Withheld, json!({"total": "5"})),
    ];
    let columns = cols(&["date", "amonut", "amount", "vendor", "total", "source"]);
    assert_eq!(empty_columns(&columns, &rows), ["amonut", "vendor", "total"]);
    assert!(empty_columns(&columns, &rows[2..]).is_empty(), "no kept row: nothing to say");
    assert!(empty_columns(&columns, &[]).is_empty());
}
