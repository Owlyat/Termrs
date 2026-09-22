//! Resolution of an `--in` source into the list of values to iterate over.

use std::fs;
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::cli::Source;

/// Resolve a source description into concrete iteration values.
///
/// Literal sources: integer literals become ranges, files become their lines,
/// and directories become the full paths of the files they directly contain.
/// Command sources run a shell command and yield its stdout lines.
pub fn values(source: &Source) -> Result<Vec<String>, String> {
    match source {
        Source::Value(value) => literal_values(value),
        Source::Command(command) => command_values(command),
    }
}

/// Resolve a literal `--in` value.
fn literal_values(source: &str) -> Result<Vec<String>, String> {
    if let Some((start, end, inclusive)) = parse_range(source) {
        return Ok(range_values(start, end, inclusive));
    }

    if let Ok(n) = source.parse::<i64>() {
        return Ok(range_values(0, n, true));
    }

    let path = Path::new(source);
    if path.is_dir() {
        return directory_values(path);
    }
    if path.is_file() {
        return file_values(path);
    }

    Err(format!("'{source}' is not an integer, file or directory"))
}

/// Parse `A..B` (exclusive) or `A..=B` (inclusive) into bounds.
fn parse_range(source: &str) -> Option<(i64, i64, bool)> {
    if let Some((a, b)) = source.split_once("..=") {
        let start = a.trim().parse().ok()?;
        let end = b.trim().parse().ok()?;
        return Some((start, end, true));
    }
    let (a, b) = source.split_once("..")?;
    let start = a.trim().parse().ok()?;
    let end = b.trim().parse().ok()?;
    Some((start, end, false))
}

/// Build the values for a numeric range.
fn range_values(start: i64, end: i64, inclusive: bool) -> Vec<String> {
    let values = if inclusive {
        (start..=end).collect::<Vec<_>>()
    } else {
        (start..end).collect::<Vec<_>>()
    };
    values.into_iter().map(|n| n.to_string()).collect()
}

/// Read a file and return its lines without trailing line endings.
fn file_values(path: &Path) -> Result<Vec<String>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("cannot read '{}': {e}", path.display()))?;

    Ok(content
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect())
}

/// List the files directly inside a directory as full paths.
fn directory_values(path: &Path) -> Result<Vec<String>, String> {
    let entries =
        fs::read_dir(path).map_err(|e| format!("cannot read '{}': {e}", path.display()))?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            files.push(entry.path().to_string_lossy().into_owned());
        }
    }

    files.sort();
    Ok(files)
}

/// Run a shell command and return its stdout lines.
fn command_values(command: &str) -> Result<Vec<String>, String> {
    let mut child = Command::new("cmd");
    #[cfg(windows)]
    child.raw_arg("/C").raw_arg(command);
    #[cfg(not(windows))]
    child.arg("/C").arg(command);

    let output = child
        .output()
        .map_err(|e| format!("failed to run '{command}': {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|line| line.trim_end_matches('\r').trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}
