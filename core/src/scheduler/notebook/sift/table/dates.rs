//! A table cell read as a date (#958), so a date column sorts by date even
//! where the sorter did not write `YYYY-MM-DD` as its prompt asks. Pure.

/// `cell` as a sort key that orders as its date: `YYYY-MM-DD`, then
/// whatever followed the date (a time, say), when the cell begins with a
/// date in a shape that can only mean one day. `None` for anything else —
/// no year (`Nov 3`), or a day and month that could be either way round
/// (`03/11/2026`) — which a date order cannot place.
pub(super) fn iso_key(cell: &str) -> Option<String> {
    let mut t = tokens(cell);
    // A leading weekday says nothing the date does not (`Tue, 3 Nov 2026`, an email's `Date:`).
    if let Some((Tok::Word(w), _)) = t.first() {
        if named(w, &WEEKDAYS).is_some() {
            t.remove(0);
        }
    }
    let ((y, m, d), end) = numeric(&t).or_else(|| worded(&t))?;
    // Only a real day: `31 Nov 2026` and `2026-02-30` are none.
    jiff::civil::Date::new(i16::try_from(y).ok()?, i8::try_from(m).ok()?, i8::try_from(d).ok()?).ok()?;
    // What followed the date, kept so times still order: glued on (`T14:05`)
    // or after one space (`, 14:05` is ` 14:05`).
    let rest = &cell[end..];
    let after = rest.trim_start_matches([' ', ',']);
    let gap = if after.is_empty() || after.len() == rest.len() { "" } else { " " };
    Some(format!("{y:04}-{m:02}-{d:02}{gap}{after}"))
}

/// A piece of a cell: a run of digits, a run of letters, or a `-`/`/`/`.`.
#[derive(Clone, Copy)]
enum Tok<'a> {
    Num(&'a str),
    Word(&'a str),
    Sep(char),
}

/// The cell's first few pieces, each with the byte offset it ends at.
/// Spaces and commas only split pieces; any other character (`:`, `$`)
/// ends the read — a date is at the start or not at all.
fn tokens(cell: &str) -> Vec<(Tok<'_>, usize)> {
    let mut out = Vec::new();
    let mut chars = cell.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if out.len() == 8 {
            break;
        }
        if c == ' ' || c == ',' {
            chars.next();
            continue;
        }
        if matches!(c, '-' | '/' | '.') {
            chars.next();
            out.push((Tok::Sep(c), start + 1));
            continue;
        }
        let digits = c.is_ascii_digit();
        if !digits && !c.is_alphabetic() {
            break;
        }
        let mut end = start;
        while let Some(&(i, d)) = chars.peek() {
            if (digits && d.is_ascii_digit()) || (!digits && d.is_alphabetic()) {
                end = i + d.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let s = &cell[start..end];
        out.push((if digits { Tok::Num(s) } else { Tok::Word(s) }, end));
    }
    out
}

/// (year, month, day) and where the date ends, from an all-number date:
/// year first (`2026-11-03`, `2026/11/3`), or year last with dots
/// (`03.11.2026`, day first as wherever dots are used), or year last with
/// `/` or `-` only when one part is over 12 (`13/11/2026`) — `03/11/2026`
/// is the 3rd of November or the 11th of March, and is not guessed.
fn numeric(t: &[(Tok<'_>, usize)]) -> Option<((i32, i32, i32), usize)> {
    use Tok::{Num, Sep};
    let short = |s: &str| s.len() <= 2;
    match t {
        [(Num(y), _), (Sep(a), _), (Num(m), _), (Sep(b), _), (Num(d), end), ..] if y.len() == 4 && a == b && short(m) && short(d) => {
            Some(((num(y), num(m), num(d)), *end))
        }
        [(Num(x), _), (Sep(a), _), (Num(z), _), (Sep(b), _), (Num(y), end), ..] if y.len() == 4 && a == b && short(x) && short(z) => {
            let (x, z) = (num(x), num(z));
            let (d, m) = match a {
                '.' => (x, z),
                _ if x > 12 => (x, z),
                _ if z > 12 => (z, x),
                _ => return None,
            };
            Some(((num(y), m, d), *end))
        }
        _ => None,
    }
}

/// (year, month, day) and where the date ends, from a date with its month
/// named: `3 Nov 2026`, `3rd November 2026`, `Nov 3, 2026`, `Sept 3rd 2026`
/// (`.` and `-` between the parts are allowed: `3. Nov. 2026`, `3-Sep-2026`).
fn worded(t: &[(Tok<'_>, usize)]) -> Option<((i32, i32, i32), usize)> {
    use Tok::{Num, Sep, Word};
    let t: Vec<_> = t.iter().filter(|(k, _)| !matches!(k, Sep('.' | '-'))).copied().collect();
    let day = |s: &str| (s.len() <= 2).then(|| num(s));
    let year = |s: &str| (s.len() == 4).then(|| num(s));
    let (d, m, y, end) = match t.as_slice() {
        [(Num(d), _), (Word(m), _), (Num(y), end), ..] => (day(d), named(m, &MONTHS), year(y), end),
        [(Num(d), _), (Word(o), _), (Word(m), _), (Num(y), end), ..] if ordinal(o) => (day(d), named(m, &MONTHS), year(y), end),
        [(Word(m), _), (Num(d), _), (Num(y), end), ..] => (day(d), named(m, &MONTHS), year(y), end),
        [(Word(m), _), (Num(d), _), (Word(o), _), (Num(y), end), ..] if ordinal(o) => (day(d), named(m, &MONTHS), year(y), end),
        _ => return None,
    };
    Some(((y?, m?, d?), *end))
}

const MONTHS: [&str; 12] =
    ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
const WEEKDAYS: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

/// `word`'s place (from 1) in `names` when it is a name or an abbreviation
/// of one at least 3 letters long (`Nov`, `Sept`; not `Novel`), any case.
fn named(word: &str, names: &[&str]) -> Option<i32> {
    let w = word.to_lowercase();
    let i = names.iter().position(|n| w.len() >= 3 && n.starts_with(&w))?;
    Some(i as i32 + 1)
}

fn ordinal(word: &str) -> bool {
    ["st", "nd", "rd", "th"].contains(&word.to_lowercase().as_str())
}

/// A run of at most 4 digits as a number (the callers check the length).
fn num(s: &str) -> i32 {
    s.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::iso_key;

    #[test]
    fn shapes_that_name_one_day_read_as_iso() {
        for (cell, key) in [
            ("2026-11-03", "2026-11-03"),
            ("2026-11-03T14:05", "2026-11-03T14:05"),
            ("2026-11-03 14:05", "2026-11-03 14:05"),
            ("2026-1-5", "2026-01-05"),
            ("2026/11/03", "2026-11-03"),
            ("03.11.2026", "2026-11-03"),
            ("3. Nov. 2026", "2026-11-03"),
            ("3 November 2026", "2026-11-03"),
            ("3-Sep-2026", "2026-09-03"),
            ("Sept 3rd, 2026", "2026-09-03"),
            ("Nov 3, 2026, 14:05", "2026-11-03 14:05"),
            ("Tue, 3 Nov 2026 14:05:00 +1100", "2026-11-03 14:05:00 +1100"),
            ("13/11/2026", "2026-11-13"),
            ("11/13/2026", "2026-11-13"),
        ] {
            assert_eq!(iso_key(cell).as_deref(), Some(key), "{cell}");
        }
    }

    #[test]
    fn shapes_that_do_not_name_one_day_are_not_read() {
        for cell in
            ["", "Nov 3", "2025", "03/11/2026", "3-10", "5-10", "3 of 10", "1.5", "$1,200", "2026-02-30", "31 Nov 2026", "Novel 3 2026", "update"]
        {
            assert_eq!(iso_key(cell), None, "{cell}");
        }
    }
}
