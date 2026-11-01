//! Path handling for source files recorded in DWARF.
//!
//! DWARF paths are kept as strings because they describe the machine that
//! produced the binary, which may use a different path syntax than the host.

/// Returns true for Unix-absolute, UNC, drive-letter and `scheme:/` paths
/// (toolchains such as wasi-sdk record sources as `wasisdk:/...`).
pub fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    if path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
    {
        return true;
    }
    path.find(":/").is_some_and(|index| {
        index >= 2
            && path[..index]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    })
}

/// Joins DWARF path fragments the way a compiler would have resolved them.
pub fn join(comp_dir: Option<&str>, dir: Option<&str>, file: &str) -> String {
    if is_absolute(file) {
        return normalize(file);
    }
    let mut base = String::new();
    match dir.filter(|d| !d.is_empty()) {
        Some(d) if is_absolute(d) => base.push_str(d),
        Some(d) => {
            if let Some(c) = comp_dir.filter(|c| !c.is_empty()) {
                base.push_str(c);
                base.push('/');
            }
            base.push_str(d);
        }
        None => {
            if let Some(c) = comp_dir.filter(|c| !c.is_empty()) {
                base.push_str(c);
            }
        }
    }
    if base.is_empty() {
        return normalize(file);
    }
    normalize(&format!("{base}/{file}"))
}

/// Uses forward slashes and resolves `.` and `..` segments lexically.
pub fn normalize(path: &str) -> String {
    let unified = path.replace('\\', "/");
    // Windows verbatim paths (`\\?\C:\...`) name the same files as plain ones.
    let unified = match unified.strip_prefix("//?/UNC/") {
        Some(rest) => format!("//{rest}"),
        None => unified
            .strip_prefix("//?/")
            .map(str::to_string)
            .unwrap_or(unified),
    };
    let (prefix, rest) = split_root(&unified);
    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|s| *s != "..") {
                    segments.pop();
                } else if prefix.is_empty() {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    format!("{prefix}{joined}")
}

fn split_root(path: &str) -> (String, &str) {
    let bytes = path.as_bytes();
    if let Some(rest) = path.strip_prefix("//") {
        return ("//".to_string(), rest);
    }
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let drive = path[..2].to_ascii_uppercase();
        let rest = &path[2..];
        return match rest.strip_prefix('/') {
            Some(r) => (format!("{drive}/"), r),
            None => (drive, rest),
        };
    }
    match path.strip_prefix('/') {
        Some(rest) => ("/".to_string(), rest),
        None => (String::new(), path),
    }
}

/// Comparison key: normalized, and case-folded on case-insensitive hosts.
pub fn key(path: &str) -> String {
    let normalized = normalize(path);
    if cfg!(windows) || cfg!(target_os = "macos") {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

/// Number of trailing path components shared by two normalized keys.
pub fn common_suffix_len(a: &str, b: &str) -> usize {
    a.rsplit('/')
        .zip(b.rsplit('/'))
        .take_while(|(x, y)| x == y && !x.is_empty())
        .count()
}

/// File name component of a DWARF path.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Bidirectional prefix mapping between build-machine and local paths.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    rules: Vec<(String, String)>,
}

impl SourceMap {
    pub fn new(rules: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut rules: Vec<(String, String)> = rules
            .into_iter()
            .map(|(from, to)| (normalize(&from), normalize(&to)))
            .collect();
        rules.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Maps a DWARF path to the local file system.
    pub fn to_local(&self, dwarf_path: &str) -> String {
        let normalized = normalize(dwarf_path);
        for (from, to) in &self.rules {
            if let Some(rest) = strip_path_prefix(&normalized, from) {
                return join_mapped(to, rest);
            }
        }
        normalized
    }

    /// Maps a local path back to the path recorded in DWARF.
    pub fn to_dwarf(&self, local_path: &str) -> Option<String> {
        let normalized = normalize(local_path);
        let mut rules: Vec<&(String, String)> = self.rules.iter().collect();
        rules.sort_by_key(|(_, to)| std::cmp::Reverse(to.len()));
        rules.into_iter().find_map(|(from, to)| {
            strip_path_prefix(&normalized, to).map(|rest| join_mapped(from, rest))
        })
    }
}

fn strip_path_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return None;
    }
    let matches = if cfg!(windows) || cfg!(target_os = "macos") {
        path.len() >= prefix.len()
            && path.is_char_boundary(prefix.len())
            && path[..prefix.len()].eq_ignore_ascii_case(prefix)
    } else {
        path.starts_with(prefix)
    };
    if !matches {
        return None;
    }
    let rest = &path[prefix.len()..];
    if rest.is_empty() {
        Some(rest)
    } else {
        rest.strip_prefix('/')
    }
}

fn join_mapped(base: &str, rest: &str) -> String {
    if rest.is_empty() {
        base.to_string()
    } else if base.ends_with('/') {
        format!("{base}{rest}")
    } else {
        format!("{base}/{rest}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_relative_file_with_comp_dir() {
        assert_eq!(
            join(Some("/build"), None, "src/main.rs"),
            "/build/src/main.rs"
        );
        assert_eq!(
            join(Some("/build"), Some("src"), "lib.rs"),
            "/build/src/lib.rs"
        );
        assert_eq!(join(Some("/build"), Some("/abs"), "x.c"), "/abs/x.c");
        assert_eq!(join(Some("/build"), None, "/other/y.rs"), "/other/y.rs");
    }

    #[test]
    fn normalizes_windows_paths_and_dot_segments() {
        assert_eq!(
            normalize(r"c:\Users\me\proj\src\..\src\.\main.rs"),
            "C:/Users/me/proj/src/main.rs"
        );
        assert_eq!(normalize("/a/b/../../c"), "/c");
        let verbatim = |rest: &str| format!("{0}{0}?{0}{rest}", '\\');
        assert_eq!(normalize(&verbatim(r"C:\src\main.rs")), "C:/src/main.rs");
        assert_eq!(
            normalize(&verbatim(r"UNC\server\share\a.rs")),
            "//server/share/a.rs"
        );
        assert_eq!(normalize("a/../../b"), "../b");
    }

    #[test]
    fn detects_absolute_paths() {
        assert!(is_absolute("/usr/src"));
        assert!(is_absolute(r"C:\src"));
        assert!(is_absolute("d:/src"));
        assert!(is_absolute("wasisdk:/v33.0+m/src/crt1.c"));
        assert_eq!(
            join(Some("wasisdk:/v33/build"), None, "wasisdk:/v33/src/a.c"),
            "wasisdk:/v33/src/a.c"
        );
        assert!(!is_absolute("src/main.rs"));
    }

    #[test]
    fn source_map_round_trips() {
        let map = SourceMap::new([("/fixtures".to_string(), "C:/work/fixtures".to_string())]);
        assert_eq!(map.to_local("/fixtures/fib.rs"), "C:/work/fixtures/fib.rs");
        assert_eq!(
            map.to_dwarf(r"c:\work\fixtures\fib.rs").as_deref(),
            Some("/fixtures/fib.rs")
        );
        assert_eq!(map.to_local("/elsewhere/a.rs"), "/elsewhere/a.rs");
        assert!(map.to_dwarf("/unrelated/a.rs").is_none());
    }

    #[test]
    fn suffix_length_counts_shared_components() {
        assert_eq!(common_suffix_len("/a/b/src/main.rs", "c:/x/src/main.rs"), 2);
        assert_eq!(common_suffix_len("/a/lib.rs", "/a/main.rs"), 0);
    }
}
