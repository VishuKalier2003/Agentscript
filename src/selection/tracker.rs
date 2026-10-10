// The Location Tracker: finds where each registered selection is now. Anchors move with the code,
// so ordinary insertions, deletions, and replacements above or below a selection, and file
// renames, are tracked without changing the selection's identifier. Resolution is strict: a
// selection resolves only when exactly one well-formed start marker and one end marker with its
// identifier exist in one file, in order, without overlapping another selection. Anything else is
// UNRESOLVED with the reason, and a plausible replacement (the baseline content found elsewhere)
// is reported as a candidate, never bound.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use super::anchors::{find_markers, language_for, normalize, Boundary, Marker, MARKER_TAG};
use super::registry::{Record, Span};
use crate::governance::workspace::Workspace;
use crate::governance::{Finding, Severity};
use crate::platform::git;

/** Largest file scanned for markers, in bytes */
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;

/** Markers found in a set of files
 * Fields
    - markers: Vec<(String, Marker)> - every marker candidate with its file path
    - texts: BTreeMap<String, String> - content of every file that contains a marker
    - files: usize - number of files scanned
*/
#[derive(Debug, Clone, Default)]
pub(crate) struct Scan {
    pub(crate) markers: Vec<(String, Marker)>,
    pub(crate) texts: BTreeMap<String, String>,
    pub(crate) files: usize,
}

impl Scan {
    /** Scan one in-memory file, such as a simulated edit or freshly inserted markers
     * Input
        - path: &str - repository-relative path, selecting the comment syntax
        - text: &str - file content
     * Output
        - Scan
    */
    pub(crate) fn single(path: &str, text: &str) -> Self {
        let mut scan = Self::default();
        scan.add(path, text);
        scan.files = 1;
        scan
    }

    /** Add the markers of one in-memory file to the scan
     * Input
        - path: &str - repository-relative path
        - text: &str - file content
     * Output
        - None (updates the scan)
    */
    fn add(&mut self, path: &str, text: &str) {
        let found = find_markers(path, text);
        if !found.is_empty() {
            self.markers
                .extend(found.into_iter().map(|marker| (path.to_string(), marker)));
            self.texts.insert(path.to_string(), text.to_string());
        }
    }
}

/** Check whether a path is Crane metadata, which never holds selections
 * Input
    - path: &str - repository-relative path
 * Output
    - bool
*/
fn is_metadata(path: &str) -> bool {
    path == ".crane" || path.starts_with(".crane/")
}

/** Scan files for markers, skipping Crane metadata, files over MAX_SCAN_BYTES, and files that are
 * not UTF-8 text
 * Input
    - workspace: &Workspace - repository
    - paths: impl IntoIterator<Item = String> - repository-relative paths
 * Output
    - Scan
*/
pub(crate) fn scan_paths(workspace: &Workspace, paths: impl IntoIterator<Item = String>) -> Scan {
    let mut scan = Scan::default();
    for path in paths {
        if is_metadata(&path) {
            continue;
        }
        let full = workspace.root.join(&path);
        if fs::metadata(&full).map_or(true, |metadata| {
            !metadata.is_file() || metadata.len() > MAX_SCAN_BYTES
        }) {
            continue;
        }
        scan.files += 1;
        let Ok(bytes) = fs::read(&full) else {
            continue;
        };
        if !bytes
            .windows(MARKER_TAG.len())
            .any(|window| window == MARKER_TAG.as_bytes())
        {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        scan.add(&path, &text);
    }
    scan
}

/** Scan every file Git tracks or would track (untracked files that are not ignored)
 * Input
    - workspace: &Workspace - repository
 * Output
    - Result<Scan, String>
    - Error if Git cannot list the files
*/
pub(crate) fn scan_repository(workspace: &Workspace) -> Result<Scan, String> {
    Ok(scan_paths(workspace, git::list_files(&workspace.root)?))
}

/** Where a selection is now
 * Fields
    - span: Span - the selected content between the markers
    - content: String - normalized selected content
    - start_marker_line: usize - line of the start marker
    - end_marker_line: usize - line of the end marker
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Location {
    pub(crate) span: Span,
    pub(crate) content: String,
    pub(crate) start_marker_line: usize,
    pub(crate) end_marker_line: usize,
}

/** The outcome of resolving one selection
 * Variants
    - Resolved(Location) - identity established
    - Unresolved { code, reason } - identity could not be established reliably
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    Resolved(Location),
    Unresolved { code: &'static str, reason: String },
}

/** Build an unresolved resolution
 * Input
    - code: &'static str - machine-readable reason
    - reason: String - explanation
 * Output
    - Resolution
*/
fn unresolved(code: &'static str, reason: String) -> Resolution {
    Resolution::Unresolved { code, reason }
}

/** Resolve a registered selection in a scan: exactly one well-formed start and one end marker in
 * one file, start before end; the content between them is the selection
 * Input
    - id: &str - selection identifier
    - scan: &Scan - markers found
 * Output
    - Resolution
*/
pub(crate) fn resolve(id: &str, scan: &Scan) -> Resolution {
    let hits = scan
        .markers
        .iter()
        .filter(|(_, marker)| marker.id.as_deref() == Some(id))
        .collect::<Vec<_>>();
    if hits.is_empty() {
        return unresolved(
            "markers_missing",
            format!("the markers of selection {id} are missing (deleted, or the file was removed)"),
        );
    }
    if let Some((path, marker)) = hits.iter().find(|(_, marker)| marker.problem.is_some()) {
        return unresolved(
            "marker_malformed",
            format!(
                "{path}:{}: {}",
                marker.line,
                marker.problem.clone().unwrap_or_default()
            ),
        );
    }
    let paths = hits
        .iter()
        .map(|(path, _)| path.as_str())
        .collect::<BTreeSet<_>>();
    if paths.len() > 1 {
        return unresolved(
            "markers_duplicated",
            format!(
                "markers of selection {id} appear in several files ({}); copied or duplicated markers are never bound",
                paths.into_iter().collect::<Vec<_>>().join(", ")
            ),
        );
    }
    let path = hits[0].0.clone();
    let starts = hits
        .iter()
        .filter(|(_, marker)| marker.boundary == Some(Boundary::Start))
        .collect::<Vec<_>>();
    let ends = hits
        .iter()
        .filter(|(_, marker)| marker.boundary == Some(Boundary::End))
        .collect::<Vec<_>>();
    let (start, end) = match (starts.as_slice(), ends.as_slice()) {
        ([start], [end]) => (&start.1, &end.1),
        ([], _) => {
            return unresolved(
                "marker_unmatched",
                format!("{path}: selection {id} has an end marker but no start marker"),
            )
        }
        (_, []) => {
            return unresolved(
                "marker_unmatched",
                format!("{path}: selection {id} has a start marker but no end marker"),
            )
        }
        _ => {
            return unresolved(
                "markers_duplicated",
                format!(
                    "{path}: selection {id} has {} start and {} end markers",
                    starts.len(),
                    ends.len()
                ),
            )
        }
    };
    if start.line >= end.line {
        return unresolved(
            "marker_inverted",
            format!("{path}: the end marker of selection {id} (line {}) comes before its start marker (line {})", end.line, start.line),
        );
    }
    // Another selection's marker between the two means overlapping or nested selections
    if let Some((_, other)) = scan.markers.iter().find(|(other_path, marker)| {
        *other_path == path
            && marker.id.as_deref() != Some(id)
            && marker.line > start.line
            && marker.line < end.line
    }) {
        return unresolved(
            "selection_overlap",
            format!(
                "{path}: selection {id} overlaps selection {} (line {})",
                other.id.clone().unwrap_or_else(|| "?".into()),
                other.line
            ),
        );
    }
    let text = &scan.texts[&path];
    let content = normalize_content(&text[start.end..end.start]);
    Resolution::Resolved(Location {
        span: Span {
            path,
            start_line: start.line + 1,
            end_line: end.line - 1,
            start_byte: start.end,
            end_byte: end.start,
        },
        content,
        start_marker_line: start.line,
        end_marker_line: end.line,
    })
}

/** Normalize selected content for digesting: CRLF becomes LF and non-empty content always ends
 * with a newline, so inserting the end marker after a final line without one is not a change
 * Input
    - content: &str - raw content between the markers
 * Output
    - String
*/
pub(crate) fn normalize_content(content: &str) -> String {
    let mut text = normalize(content);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/** Report scan-level problems that do not belong to one registered selection: malformed markers
 * without a readable identifier and markers whose identifier is not in the registry (forged,
 * copied from another repository, or left behind)
 * Input
    - scan: &Scan - markers found
    - registered: &BTreeSet<String> - registered identifiers
 * Output
    - Vec<Finding>
*/
pub(crate) fn scan_findings(scan: &Scan, registered: &BTreeSet<String>) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut reported = BTreeSet::new();
    for (path, marker) in &scan.markers {
        match &marker.id {
            None => findings.push(
                Finding::new(
                    "marker_malformed",
                    Severity::High,
                    format!(
                        "{path}:{}: {}",
                        marker.line,
                        marker.problem.clone().unwrap_or_default()
                    ),
                )
                .with_path(path),
            ),
            Some(id) if !registered.contains(id) && reported.insert((path.clone(), id.clone())) => {
                findings.push(
                    Finding::new(
                        "marker_unregistered",
                        Severity::Critical,
                        format!(
                            "{path}:{}: marker {id} is not in the signed registry (forged, copied, or orphaned); it grants nothing",
                            marker.line
                        ),
                    )
                    .with_selection(id)
                    .with_path(path),
                )
            }
            _ => {}
        }
    }
    findings
}

/** Look for the baseline content of a selection whose markers are missing, so a human can see
 * where the code went; the result is a hint only and is never bound automatically
 * Input
    - workspace: &Workspace - repository
    - record: &Record - selection record
 * Output
    - Vec<String> "path:line" locations of exact baseline matches (at most 5)
*/
pub(crate) fn candidates(workspace: &Workspace, record: &Record) -> Vec<String> {
    let Ok(Some(bytes)) = git::show(
        &workspace.root,
        &record.origin.commit,
        &record.origin.span.path,
    ) else {
        return Vec::new();
    };
    let baseline = String::from_utf8_lossy(&bytes);
    let Some(original) = baseline.get(record.origin.span.start_byte..record.origin.span.end_byte)
    else {
        return Vec::new();
    };
    let original = normalize(original);
    if original.trim().is_empty() {
        return Vec::new();
    }
    let Ok(files) = git::list_files(&workspace.root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for path in files.into_iter().filter(|path| !is_metadata(path)) {
        if language_for(&path).is_none() {
            continue;
        }
        let Ok(text) = fs::read_to_string(workspace.root.join(&path)) else {
            continue;
        };
        let text = normalize(&text);
        if let Some(offset) = text.find(&original) {
            found.push(format!(
                "{path}:{}",
                text[..offset].matches('\n').count() + 1
            ));
            if found.len() == 5 {
                break;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::anchors::{insert, CommentSyntax};

    /** Build a scan of in-memory files
     * Input
        - files: &[(&str, &str)] - path and content pairs
     * Output
        - Scan
    */
    fn scan_of(files: &[(&str, &str)]) -> Scan {
        let mut scan = Scan::default();
        for (path, text) in files {
            scan.add(path, text);
        }
        scan
    }

    /** Check resolution after edits above the selection, after a rename, and for every
     * unresolved case
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn resolves_strictly() {
        let syntax = CommentSyntax::Line("#");
        let text = insert("a = 1\nb = 2\nc = 3\n", syntax, "AAAAAAAA", 2, 2).unwrap();
        let Resolution::Resolved(location) = resolve("AAAAAAAA", &scan_of(&[("x.py", &text)]))
        else {
            panic!("expected resolution");
        };
        assert_eq!(location.content, "b = 2\n");
        assert_eq!(location.span.start_line, 3);
        let shifted = format!("new = 0\nnew = 1\n{text}");
        let Resolution::Resolved(moved) =
            resolve("AAAAAAAA", &scan_of(&[("renamed.py", &shifted)]))
        else {
            panic!("expected resolution after insertion and rename");
        };
        assert_eq!(
            (moved.span.path.as_str(), moved.span.start_line),
            ("renamed.py", 5)
        );
        assert_eq!(moved.content, location.content);
        let unresolved_code = |files: &[(&str, &str)]| match resolve("AAAAAAAA", &scan_of(files)) {
            Resolution::Unresolved { code, .. } => code,
            Resolution::Resolved(_) => "resolved",
        };
        assert_eq!(unresolved_code(&[("x.py", "a\n")]), "markers_missing");
        assert_eq!(
            unresolved_code(&[("x.py", &text), ("y.py", &text)]),
            "markers_duplicated"
        );
        let start_only = text.replace("# @crane:selection:AAAAAAAA:end\n", "");
        assert_eq!(
            unresolved_code(&[("x.py", &start_only)]),
            "marker_unmatched"
        );
        let inverted = "# @crane:selection:AAAAAAAA:end\nx\n# @crane:selection:AAAAAAAA:start\n";
        assert_eq!(unresolved_code(&[("x.py", inverted)]), "marker_inverted");
        assert_eq!(unresolved_code(&[("x.js", &text)]), "marker_malformed");
        let nested = insert(&text, syntax, "BBBBBBBB", 3, 3).unwrap();
        assert_eq!(unresolved_code(&[("x.py", &nested)]), "selection_overlap");
    }

    /** Check that unregistered and unreadable markers are reported
     * Input
        - None
     * Output
        - None (panics on failure)
    */
    #[test]
    fn reports_unregistered_markers() {
        let text = insert("a\n", CommentSyntax::Line("#"), "ZZZZZZZZ", 1, 1).unwrap();
        let scan = scan_of(&[("x.py", &text), ("y.py", "# @crane:selection:bad:start\n")]);
        let findings = scan_findings(&scan, &BTreeSet::new());
        assert_eq!(findings.len(), 2);
        assert!(findings
            .iter()
            .any(|finding| finding.code == "marker_unregistered"));
        assert!(findings
            .iter()
            .any(|finding| finding.code == "marker_malformed"));
    }
}
