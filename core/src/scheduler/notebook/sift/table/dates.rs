//! A table cell read as a date (#958), so a date column sorts by date even
//! where the sorter did not write `YYYY-MM-DD` as its prompt asks. Pure.

/// `cell` as a sort key that orders as its date: `YYYY-MM-DD`, then
/// ` HH:MM:SS` when a time follows the date (24-hour, and in UTC when it
/// carries an offset), when the cell begins with a date in a shape that can
/// only mean one day. Anything else after the date is not in the key. `None`
/// for a cell that does not begin so — no year (`Nov 3`), or a day and
/// month that could be either way round (`03/11/2026`) — which a date order
/// cannot place.
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
    let date = jiff::civil::Date::new(i16::try_from(y).ok()?, i8::try_from(m).ok()?, i8::try_from(d).ok()?).ok()?;
    // A time is ordered as a time, not as the text it was written in (review
    // finding: `T09:00` sorted after ` 14:05`, `9:00` after `10:00`); a time
    // that does not read leaves the day alone, tied with the rest of it.
    let Some((h, mi, s, offset)) = time(&cell[end..]) else {
        return Some(date.to_string());
    };
    let Ok(at) = jiff::civil::DateTime::new(date.year(), date.month(), date.day(), h, mi, s, 0) else {
        return Some(date.to_string());
    };
    // shortcut: a time with no offset is taken as UTC, so it orders against an
    // offset one only to within the writer's zone; read a zone from the task if that matters.
    let utc = offset
        .and_then(|o| jiff::tz::Offset::from_seconds(o).ok())
        .and_then(|o| at.to_zoned(jiff::tz::TimeZone::fixed(o)).ok())
        .map_or(at, |z| z.with_time_zone(jiff::tz::TimeZone::UTC).datetime());
    Some(utc.strftime("%Y-%m-%d %H:%M:%S").to_string())
}

/// The time at the start of `rest`, what followed a date: (hour, minute,
/// second, UTC offset in seconds) from `14:05`, `T14:05:00.250Z`,
/// `, 9:00 AM`, `14:05:00 +1100`, `23:30 -08:00`. `None` when no
/// `H:MM` starts it.
fn time(rest: &str) -> Option<(i8, i8, i8, Option<i32>)> {
    let r = rest.trim_start_matches([' ', ',']);
    let r = r.strip_prefix(['T', 't']).unwrap_or(r);
    let (h, r) = digits(r, 1, 2)?;
    let (mi, r) = digits(r.strip_prefix(':')?, 2, 2)?;
    let (s, r) = r.strip_prefix(':').and_then(|r| digits(r, 2, 2)).unwrap_or((0, r));
    let r = r.strip_prefix('.').map_or(r, |f| f.trim_start_matches(|c: char| c.is_ascii_digit())).trim_start();
    let half = r.get(..2).map(str::to_ascii_lowercase);
    let (h, r) = match half.as_deref() {
        Some(p @ ("am" | "pm")) if (1..=12).contains(&h) => (h % 12 + if p == "pm" { 12 } else { 0 }, r[2..].trim_start()),
        _ => (h, r),
    };
    let offset = if r.starts_with(['Z', 'z']) || r.starts_with("UTC") || r.starts_with("GMT") {
        Some(0)
    } else if let Some((sign, r)) = r.strip_prefix('+').map(|r| (1, r)).or_else(|| r.strip_prefix('-').map(|r| (-1, r))) {
        digits(r, 2, 2).map(|(oh, r)| {
            let om = digits(r.strip_prefix(':').unwrap_or(r), 2, 2).map_or(0, |(m, _)| i32::from(m));
            sign * (i32::from(oh) * 3600 + om * 60)
        })
    } else {
        None
    };
    Some((h, mi, s, offset))
}

/// The number in the first `min..=max` digits of `s` (as many as there
/// are, up to `max`), and what follows them.
fn digits(s: &str, min: usize, max: usize) -> Option<(i8, &str)> {
    let n = s.bytes().take(max).take_while(u8::is_ascii_digit).count();
    (n >= min).then(|| (s[..n].parse().unwrap_or(0), &s[n..]))
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
            ("2026-11-03T14:05", "2026-11-03 14:05:00"),
            ("2026-11-03 14:05", "2026-11-03 14:05:00"),
            ("2026-1-5", "2026-01-05"),
            ("2026/11/03", "2026-11-03"),
            ("03.11.2026", "2026-11-03"),
            ("3. Nov. 2026", "2026-11-03"),
            ("3 November 2026", "2026-11-03"),
            ("3rd November 2026", "2026-11-03"),
            ("4th Nov 2026", "2026-11-04"),
            ("3-Sep-2026", "2026-09-03"),
            ("Sept 3rd, 2026", "2026-09-03"),
            ("Tue, 3rd Nov. 2026", "2026-11-03"),
            ("3 Nov 2026,", "2026-11-03"),
            ("Nov 3, 2026, 14:05", "2026-11-03 14:05:00"),
            ("Nov 3, 2026, 9:00 AM", "2026-11-03 09:00:00"),
            ("Nov 3, 2026, 12:30 am", "2026-11-03 00:30:00"),
            ("Nov 3, 2026 2:00 PM", "2026-11-03 14:00:00"),
            ("2026-11-03T14:05:00.250Z", "2026-11-03 14:05:00"),
            ("Tue, 3 Nov 2026 14:05:00 +1100", "2026-11-03 03:05:00"),
            ("Tue, 3 Nov 2026 23:30:00 -08:00", "2026-11-04 07:30:00"),
            ("2026-11-03 (about)", "2026-11-03"),
            ("13/11/2026", "2026-11-13"),
            ("11/13/2026", "2026-11-13"),
        ] {
            assert_eq!(iso_key(cell).as_deref(), Some(key), "{cell}");
        }
    }

    /// Review finding: keys of one day in different shapes order by time,
    /// not by the text after the date (`T` sorts after a space, `9` after `1`).
    #[test]
    fn times_order_whatever_their_shape() {
        let k = |c: &str| iso_key(c).unwrap();
        assert!(k("2026-11-03T09:00") < k("3 Nov 2026 14:05"));
        assert!(k("Nov 3, 2026, 9:00") < k("Nov 3, 2026, 10:00"));
        assert!(k("Nov 3, 2026, 9:00 AM") < k("Nov 3, 2026, 2:00 PM"));
        assert!(k("Wed, 4 Nov 2026 01:00:00 +0000") < k("Tue, 3 Nov 2026 23:30:00 -0800"));
    }

    #[test]
    fn shapes_that_do_not_name_one_day_are_not_read() {
        for cell in [
            "",
            "Nov 3",
            "2025",
            "03/11/2026",
            "03-11-2026",
            "03.11.26",
            "11/03/26",
            "3 Nov 26",
            "3-10",
            "5-10",
            "3 of 10",
            "3 of November 2026",
            "Nov 3 of 2026",
            "J 3 2026",
            "Ma 3 2026",
            "1.5",
            "$1,200",
            "2026-02-30",
            "31 Nov 2026",
            "Novel 3 2026",
            "update",
        ] {
            assert_eq!(iso_key(cell), None, "{cell}");
        }
    }
}
