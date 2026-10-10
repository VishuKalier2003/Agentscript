// Line diff (Myers' O(ND) algorithm) used to map line ranges between two versions of a file: the
// work tree and its checkpoint version when a selection is created, so a range is only accepted
// when every selected line is unchanged since the trusted checkpoint.

/** For each line of the new version, the line of the old version it is equal to, if any
 * Input
    - old: &[&str] - lines of the old version
    - new: &[&str] - lines of the new version
 * Output
    - Vec<Option<usize>> of length new.len(), 0-based old line indexes
*/
pub(crate) fn map_lines(old: &[&str], new: &[&str]) -> Vec<Option<usize>> {
    let mut mapping = vec![None; new.len()];
    // Equal prefix and suffix are mapped directly, keeping the search small for local edits
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        mapping[prefix] = Some(prefix);
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        mapping[new.len() - 1 - suffix] = Some(old.len() - 1 - suffix);
        suffix += 1;
    }
    let old_middle = &old[prefix..old.len() - suffix];
    let new_middle = &new[prefix..new.len() - suffix];
    for (old_index, new_index) in myers(old_middle, new_middle) {
        mapping[prefix + new_index] = Some(prefix + old_index);
    }
    mapping
}

/** Find a longest common subsequence of lines with Myers' algorithm, keeping each round's
 * frontier for the backtrack
 * Input
    - old: &[&str] - old lines
    - new: &[&str] - new lines
 * Output
    - Vec<(usize, usize)> pairs of equal (old index, new index), in order
*/
fn myers(old: &[&str], new: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (old.len() as isize, new.len() as isize);
    if n == 0 || m == 0 {
        return Vec::new();
    }
    let max = (n + m) as usize;
    let offset = max as isize;
    let mut frontier = vec![0isize; 2 * max + 2];
    let mut trace = Vec::new();
    'search: for depth in 0..=max as isize {
        trace.push(frontier.clone());
        let mut k = -depth;
        while k <= depth {
            let index = (k + offset) as usize;
            let mut x = if k == -depth || (k != depth && frontier[index - 1] < frontier[index + 1])
            {
                frontier[index + 1]
            } else {
                frontier[index - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            frontier[index] = x;
            if x >= n && y >= m {
                break 'search;
            }
            k += 2;
        }
    }
    // Backtrack through the saved frontiers, collecting the diagonal (equal) moves
    let mut pairs = Vec::new();
    let (mut x, mut y) = (n, m);
    for depth in (0..trace.len() as isize).rev() {
        let frontier = &trace[depth as usize];
        let k = x - y;
        let previous_k = if k == -depth
            || (k != depth
                && frontier[(k - 1 + offset) as usize] < frontier[(k + 1 + offset) as usize])
        {
            k + 1
        } else {
            k - 1
        };
        let previous_x = if depth == 0 {
            0
        } else {
            frontier[(previous_k + offset) as usize]
        };
        let previous_y = previous_x - previous_k;
        while x > previous_x && y > previous_y {
            x -= 1;
            y -= 1;
            pairs.push((x as usize, y as usize));
        }
        if depth == 0 {
            break;
        }
        x = previous_x;
        y = previous_y;
    }
    pairs.reverse();
    pairs
}

#[cfg(test)]
mod tests {
    use super::map_lines;

    /** Check mapping across insertions, deletions, and replacements
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn maps_unchanged_lines() {
        let old = ["a", "b", "c", "d", "e"];
        let new = ["x", "a", "b", "C", "d", "e", "y"];
        assert_eq!(
            map_lines(&old, &new),
            vec![None, Some(0), Some(1), None, Some(3), Some(4), None]
        );
        assert_eq!(map_lines(&old, &old), (0..5).map(Some).collect::<Vec<_>>());
        assert_eq!(map_lines(&[], &["a"]), vec![None]);
        let shuffled = map_lines(&["a", "b", "a", "b"], &["b", "a", "b", "a"]);
        assert_eq!(shuffled.iter().flatten().count(), 3);
    }
}
