//! The order Godot's editor registers global script classes in, so a `class_name` declared by two
//! files resolves to the same file it does in Godot.
//!
//! `EditorFileSystem::_first_scan_process_scripts` (editor_file_system.cpp:363-395) walks the
//! project post-order: every subdirectory first, then the directory's own files, each list sorted
//! with `FileNoCaseComparator` (`String::filenocasecmp_to`, ustring.cpp:691). Every script calls
//! `ScriptServer::add_global_class`, which overwrites an existing entry (script_language.cpp:408),
//! so of several files declaring one name, the **last** one in this order wins.

use std::cmp::Ordering;

use camino::Utf8Path;

/// Compare two files by when Godot's first scan reaches them. `Ordering::Greater` means `a` is
/// registered later, so `a` wins a `class_name` both declare.
///
/// Names `filenocasecmp_to` calls equal (`A.gd` and `a.gd`) fall back to a byte comparison, so the
/// answer never depends on insertion order.
#[must_use]
pub fn godot_scan_order(a: &Utf8Path, b: &Utf8Path) -> Ordering {
    let a_parts: Vec<&str> = a.components().map(|c| c.as_str()).collect();
    let b_parts: Vec<&str> = b.components().map(|c| c.as_str()).collect();
    let shared = a_parts
        .iter()
        .zip(&b_parts)
        .take_while(|(x, y)| x == y)
        .count();
    if shared == a_parts.len() || shared == b_parts.len() {
        return a_parts.len().cmp(&b_parts.len());
    }
    let a_is_file_here = shared + 1 == a_parts.len();
    let b_is_file_here = shared + 1 == b_parts.len();
    match (a_is_file_here, b_is_file_here) {
        // A file in this directory is reached after everything in its subdirectories.
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => filenocasecmp(a_parts[shared], b_parts[shared])
            .then_with(|| a_parts[shared].cmp(b_parts[shared])),
    }
}

/// `String::filenocasecmp_to` (ustring.cpp:691): leading `_` sorts before everything but `.`, then
/// a case-insensitive natural comparison.
fn filenocasecmp(a: &str, b: &str) -> Ordering {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (mut i, mut j) = (0, 0);
    if let Some(ord) = compare_leading_underscores(&a, &b, &mut i, &mut j) {
        return ord;
    }
    natural_nocase_cmp(&a[i..], &b[j..])
}

/// `file_cmp_common` (ustring.cpp:651).
fn compare_leading_underscores(
    a: &[char],
    b: &[char],
    i: &mut usize,
    j: &mut usize,
) -> Option<Ordering> {
    loop {
        let (x, y) = (a.get(*i).copied(), b.get(*j).copied());
        let either_underscore = (x == Some('_') && y.is_some()) || (x.is_some() && y == Some('_'));
        if !either_underscore {
            return None;
        }
        if x != Some('_') {
            return Some(if x == Some('.') {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        if y != Some('_') {
            return Some(if y == Some('.') {
                Ordering::Greater
            } else {
                Ordering::Less
            });
        }
        *i += 1;
        *j += 1;
    }
}

/// `naturalnocasecmp_to_base` (ustring.cpp:603).
fn natural_nocase_cmp(a: &[char], b: &[char]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while a.get(i) == Some(&'.') || b.get(j) == Some(&'.') {
        let x = a.get(i).copied();
        let y = b.get(j).copied();
        i += 1;
        j += 1;
        if x != Some('.') {
            return Ordering::Greater;
        }
        if y != Some('.') {
            return Ordering::Less;
        }
        if j >= b.len() {
            return Ordering::Greater;
        }
        if i >= a.len() {
            return Ordering::Less;
        }
    }
    while i < a.len() {
        let Some(&y) = b.get(j) else {
            return Ordering::Greater;
        };
        let x = a[i];
        if x.is_ascii_digit() {
            if !y.is_ascii_digit() {
                return Ordering::Less;
            }
            let ord = compare_digit_runs(a, b, &mut i, &mut j);
            if ord != Ordering::Equal {
                return ord;
            }
        } else if y.is_ascii_digit() {
            return Ordering::Greater;
        } else {
            let ord = to_upper(x).cmp(&to_upper(y));
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
    }
    if j < b.len() {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

/// `natural_cmp_common` (ustring.cpp:508): longer number (ignoring leading zeros) is greater, then
/// digit by digit. Advances both cursors past their digit runs.
fn compare_digit_runs(a: &[char], b: &[char], i: &mut usize, j: &mut usize) -> Ordering {
    let a_end = digit_run_end(a, *i);
    let b_end = digit_run_end(b, *j);
    let a_digits = strip_leading_zeros(&a[*i..a_end]);
    let b_digits = strip_leading_zeros(&b[*j..b_end]);
    *i = a_end;
    *j = b_end;
    a_digits
        .len()
        .cmp(&b_digits.len())
        .then_with(|| a_digits.cmp(b_digits))
}

fn digit_run_end(s: &[char], start: usize) -> usize {
    start + s[start..].iter().take_while(|c| c.is_ascii_digit()).count()
}

fn strip_leading_zeros(digits: &[char]) -> &[char] {
    let zeros = digits.iter().take_while(|&&c| c == '0').count();
    &digits[zeros..]
}

/// `_find_upper` (ucaps.h): the simple uppercase mapping, or the char itself.
fn to_upper(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(a: &str, b: &str) -> Ordering {
        godot_scan_order(Utf8Path::new(a), Utf8Path::new(b))
    }

    #[test]
    fn a_file_comes_after_everything_in_sibling_subdirectories() {
        assert_eq!(order("/p/z.gd", "/p/a/b.gd"), Ordering::Greater);
        assert_eq!(order("/p/a/b.gd", "/p/z.gd"), Ordering::Less);
    }

    #[test]
    fn siblings_sort_case_insensitively() {
        assert_eq!(order("/p/B.gd", "/p/a.gd"), Ordering::Greater);
        assert_eq!(order("/p/x/a.gd", "/p/Y/a.gd"), Ordering::Less);
    }

    #[test]
    fn numbers_sort_naturally() {
        assert_eq!(order("/p/item10.gd", "/p/item9.gd"), Ordering::Greater);
        assert_eq!(order("/p/item009.gd", "/p/item10.gd"), Ordering::Less);
    }

    #[test]
    fn a_leading_underscore_sorts_first() {
        assert_eq!(order("/p/_a.gd", "/p/A.gd"), Ordering::Less);
        assert_eq!(order("/p/_b/x.gd", "/p/a/x.gd"), Ordering::Less);
    }

    #[test]
    fn case_only_differences_still_order_deterministically() {
        assert_ne!(order("/p/A.gd", "/p/a.gd"), Ordering::Equal);
        assert_eq!(
            order("/p/A.gd", "/p/a.gd"),
            order("/p/a.gd", "/p/A.gd").reverse()
        );
    }

    #[test]
    fn the_same_path_is_equal() {
        assert_eq!(order("/p/a/b.gd", "/p/a/b.gd"), Ordering::Equal);
    }
}
