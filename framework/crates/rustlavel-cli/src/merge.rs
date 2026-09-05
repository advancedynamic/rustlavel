//! Three-way merge: base, yours, theirs.
//!
//! `rustlavel upgrade` has to reconcile three versions of every file the kit
//! owns — the one the old CLI wrote (*base*), the one in the project now
//! (*yours*), and the one this CLI would write (*theirs*). Where only one side
//! moved, the answer is that side. Where both moved to the same place, the
//! answer is that place. Where both moved differently, there is no answer, and
//! saying so out loud is the only honest thing to do.
//!
//! This is the classic diff3, and it is written here rather than shelled out
//! to `git merge-file` for two reasons: the CLI depends on nothing and is
//! worth keeping that way, and a merge that only works inside a git work tree
//! would not work for the people who need it most.
//!
//! **Lines keep their endings.** Everything below splits with
//! `split_inclusive('\n')`, so a file with CRLF endings, or one with no final
//! newline, comes back out exactly as it went in. Joining is concatenation.

/// What a merge did, for the report the command prints afterwards.
#[derive(Debug, PartialEq, Eq)]
pub struct Merged {
    pub text: String,
    /// How many places both sides changed differently. Zero means the file was
    /// merged cleanly and can be written without anybody reading it.
    pub conflicts: usize,
}

/// Merge `ours` and `theirs`, both descended from `base`.
///
/// `label_ours` and `label_theirs` name the two sides in a conflict marker;
/// they are shown to somebody reading the file in an editor, so they should
/// say where the text came from rather than which argument it was.
pub fn merge(
    base: &str,
    ours: &str,
    theirs: &str,
    label_ours: &str,
    label_theirs: &str,
) -> Merged {
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let o: Vec<&str> = ours.split_inclusive('\n').collect();
    let t: Vec<&str> = theirs.split_inclusive('\n').collect();

    // Where each base line ended up on each side, or `None` if that side
    // deleted or replaced it. A line matched on *both* sides is a place all
    // three agree, which is where a chunk can safely start and end.
    let in_ours = align(&b, &o);
    let in_theirs = align(&b, &t);

    let mut text = String::with_capacity(theirs.len());
    let mut conflicts = 0;
    let (mut bi, mut oi, mut ti) = (0usize, 0usize, 0usize);

    loop {
        // Stable run: the same line, in the same place, in all three.
        while bi < b.len() {
            match (in_ours[bi], in_theirs[bi]) {
                (Some(x), Some(y)) if x == oi && y == ti => {
                    text.push_str(b[bi]);
                    bi += 1;
                    oi += 1;
                    ti += 1;
                }
                _ => break,
            }
        }
        if bi >= b.len() && oi >= o.len() && ti >= t.len() {
            break;
        }

        // The next place all three line up again. Everything before it is one
        // unstable region, resolved as a whole rather than line by line —
        // resolving line by line is how a merge produces text neither side
        // wrote.
        let (nb, no, nt) = next_stable(&b, &in_ours, &in_theirs, bi, oi, ti, o.len(), t.len());

        let base_slice = &b[bi..nb];
        let ours_slice = &o[oi..no];
        let theirs_slice = &t[ti..nt];

        if same(ours_slice, base_slice) {
            // Only they moved.
            push(&mut text, theirs_slice);
        } else if same(theirs_slice, base_slice) {
            // Only you moved.
            push(&mut text, ours_slice);
        } else if same(ours_slice, theirs_slice) {
            // Both moved, to the same place.
            push(&mut text, ours_slice);
        } else {
            conflicts += 1;
            conflict(&mut text, ours_slice, theirs_slice, label_ours, label_theirs);
        }

        bi = nb;
        oi = no;
        ti = nt;
    }

    Merged { text, conflicts }
}

/// The first index at or after `bi` where both sides still hold the base line,
/// together with where that line sits on each side.
///
/// Falling off the end is a stable point too: it is where all three files run
/// out, and the tail before it is one last region to resolve.
#[allow(clippy::too_many_arguments)]
fn next_stable(
    b: &[&str],
    in_ours: &[Option<usize>],
    in_theirs: &[Option<usize>],
    bi: usize,
    oi: usize,
    ti: usize,
    ours_len: usize,
    theirs_len: usize,
) -> (usize, usize, usize) {
    for k in bi..b.len() {
        if let (Some(x), Some(y)) = (in_ours[k], in_theirs[k]) {
            // Only a match that lies ahead of where each side already is can
            // close the region; one behind would mean walking backwards.
            if x >= oi && y >= ti {
                return (k, x, y);
            }
        }
    }
    (b.len(), ours_len, theirs_len)
}

fn same(a: &[&str], b: &[&str]) -> bool {
    a == b
}

fn push(text: &mut String, lines: &[&str]) {
    for line in lines {
        text.push_str(line);
    }
}

/// Write the region both sides changed, in git's marker format.
///
/// A file with these in it does not compile, and that is the point: an upgrade
/// that could not decide must not be able to pass unnoticed.
fn conflict(
    text: &mut String,
    ours: &[&str],
    theirs: &[&str],
    label_ours: &str,
    label_theirs: &str,
) {
    ensure_newline(text);
    text.push_str("<<<<<<< ");
    text.push_str(label_ours);
    text.push('\n');
    push(text, ours);
    ensure_newline(text);
    text.push_str("=======\n");
    push(text, theirs);
    ensure_newline(text);
    text.push_str(">>>>>>> ");
    text.push_str(label_theirs);
    text.push('\n');
}

/// A marker has to start its own line, even when the text before it ended
/// without a newline — which is exactly the case at the end of a file.
fn ensure_newline(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

/// For each line of `a`, where it sits in `b` — or `None` if `b` does not keep
/// it in that order.
///
/// This is a longest-common-subsequence, computed on a shrunken problem: the
/// shared head and tail come off first, because almost every real edit leaves
/// most of a file untouched and the table only has to cover what moved.
fn align(a: &[&str], b: &[&str]) -> Vec<Option<usize>> {
    let mut out = vec![None; a.len()];

    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        out[head] = Some(head);
        head += 1;
    }

    let mut tail = 0;
    while tail < a.len() - head && tail < b.len() - head && a[a.len() - 1 - tail] == b[b.len() - 1 - tail] {
        out[a.len() - 1 - tail] = Some(b.len() - 1 - tail);
        tail += 1;
    }

    let a_mid = &a[head..a.len() - tail];
    let b_mid = &b[head..b.len() - tail];
    if a_mid.is_empty() || b_mid.is_empty() {
        return out;
    }

    // The table is (n+1)(m+1) cells. A pair too big to fit the budget is left
    // entirely unmatched, which turns the file into one region and — unless
    // one side left it alone — one conflict. A slow correct answer would be
    // better; a fast wrong one would not, and neither would waiting a minute
    // on a generated stylesheet somebody has never opened.
    const BUDGET: usize = 4_000_000;
    if a_mid.len().saturating_mul(b_mid.len()) > BUDGET {
        return out;
    }

    let n = a_mid.len();
    let m = b_mid.len();
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * (m + 1) + j] = if a_mid[i] == b_mid[j] {
                table[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                table[(i + 1) * (m + 1) + j].max(table[i * (m + 1) + j + 1])
            };
        }
    }

    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a_mid[i] == b_mid[j] {
            out[head + i] = Some(head + j);
            i += 1;
            j += 1;
        } else if table[(i + 1) * (m + 1) + j] >= table[i * (m + 1) + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }

    out
}

/// Does this text carry markers a merge left behind?
///
/// `upgrade` refuses to start when it does: merging on top of an unfinished
/// merge produces something nobody can untangle.
pub fn has_conflict_markers(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("<<<<<<< ") || line == "=======" || line.starts_with(">>>>>>> "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(base: &str, ours: &str, theirs: &str) -> Merged {
        merge(base, ours, theirs, "yours", "new")
    }

    #[test]
    fn a_file_nobody_touched_comes_back_unchanged() {
        let text = "one\ntwo\nthree\n";
        assert_eq!(merged(text, text, text).text, text);
        assert_eq!(merged(text, text, text).conflicts, 0);
    }

    #[test]
    fn a_change_only_the_new_version_made_is_taken() {
        let base = "one\ntwo\nthree\n";
        let theirs = "one\nTWO\nthree\n";
        let result = merged(base, base, theirs);
        assert_eq!(result.text, theirs);
        assert_eq!(result.conflicts, 0);
    }

    #[test]
    fn a_change_only_you_made_is_kept() {
        let base = "one\ntwo\nthree\n";
        let ours = "one\nmine\nthree\n";
        let result = merged(base, ours, base);
        assert_eq!(result.text, ours);
        assert_eq!(result.conflicts, 0);
    }

    /// The case the whole feature exists for: an upgrade adds a line at the
    /// top of a file you edited at the bottom, and you lose nothing.
    #[test]
    fn edits_in_different_places_both_survive() {
        let base = "one\ntwo\nthree\n";
        let ours = "one\ntwo\nthree\nmine\n";
        let theirs = "zero\none\ntwo\nthree\n";
        let result = merged(base, ours, theirs);
        assert_eq!(result.conflicts, 0);
        assert_eq!(result.text, "zero\none\ntwo\nthree\nmine\n");
    }

    #[test]
    fn the_same_edit_made_twice_is_not_a_conflict() {
        let base = "one\ntwo\n";
        let both = "one\nchanged\n";
        let result = merged(base, both, both);
        assert_eq!(result.conflicts, 0);
        assert_eq!(result.text, both);
    }

    #[test]
    fn two_different_edits_to_one_line_conflict() {
        let base = "one\ntwo\nthree\n";
        let ours = "one\nmine\nthree\n";
        let theirs = "one\ntheirs\nthree\n";
        let result = merged(base, ours, theirs);
        assert_eq!(result.conflicts, 1);
        assert_eq!(
            result.text,
            "one\n<<<<<<< yours\nmine\n=======\ntheirs\n>>>>>>> new\nthree\n"
        );
    }

    /// A conflict has to be visible to the compiler, not only to a reader.
    #[test]
    fn a_conflict_leaves_markers_the_guard_can_find() {
        let result = merged("a\n", "b\n", "c\n");
        assert!(has_conflict_markers(&result.text));
        assert!(!has_conflict_markers("a\nb\n"));
    }

    #[test]
    fn a_deletion_on_one_side_is_taken() {
        let base = "one\ntwo\nthree\n";
        let theirs = "one\nthree\n";
        assert_eq!(merged(base, base, theirs).text, theirs);
        assert_eq!(merged(base, theirs, base).text, theirs);
    }

    /// Not every file ends in a newline, and a merge must not invent one in
    /// the middle of the text or lose one at the end.
    #[test]
    fn a_file_with_no_final_newline_keeps_it_that_way() {
        let base = "one\ntwo";
        let ours = "one\nmine";
        let result = merged(base, ours, base);
        assert_eq!(result.text, "one\nmine");
    }

    #[test]
    fn crlf_endings_survive() {
        let base = "one\r\ntwo\r\n";
        let theirs = "one\r\nTWO\r\n";
        let result = merged(base, base, theirs);
        assert_eq!(result.text, theirs);
    }

    /// An empty base is the "both created the same path" case, and it must not
    /// silently pick one.
    #[test]
    fn two_versions_of_a_file_that_did_not_exist_conflict() {
        let result = merged("", "mine\n", "theirs\n");
        assert_eq!(result.conflicts, 1);
    }

    /// A file too large to align is left as one region rather than merged
    /// wrongly — but only when both sides moved. A one-sided change to a huge
    /// file still applies.
    #[test]
    fn an_enormous_file_still_takes_a_one_sided_change() {
        let base: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        let mut theirs = base.clone();
        theirs.push_str("added\n");
        let result = merged(&base, &base, &theirs);
        assert_eq!(result.conflicts, 0);
        assert_eq!(result.text, theirs);
    }
}
