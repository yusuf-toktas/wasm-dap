use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::paths;

/// Identifier of a source file inside a [`LineTable`].
pub type FileId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRow {
    /// Module offset of the first instruction covered by the row.
    pub address: u32,
    pub file: FileId,
    /// One-based line; zero means "no source line" (compiler generated code).
    pub line: u32,
    /// One-based column; zero means unknown.
    pub column: u32,
    pub is_stmt: bool,
    pub prologue_end: bool,
}

#[derive(Debug, Clone)]
struct Sequence {
    start: u32,
    end: u32,
    rows: Vec<LineRow>,
}

/// Source location of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLine {
    pub file: FileId,
    pub line: u32,
    pub column: u32,
}

/// Resolution of a requested source breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLine {
    pub line: u32,
    pub column: u32,
    /// One module offset per function that contains code for the line.
    pub addresses: Vec<u32>,
}

/// Address-to-line mapping merged from every compilation unit.
#[derive(Debug, Clone, Default)]
pub struct LineTable {
    files: Vec<String>,
    file_index: HashMap<String, FileId>,
    sequences: Vec<Sequence>,
}

impl LineTable {
    pub(crate) fn intern_file(&mut self, path: String) -> FileId {
        if let Some(id) = self.file_index.get(&path) {
            return *id;
        }
        let id = self.files.len() as FileId;
        self.file_index.insert(path.clone(), id);
        self.files.push(path);
        id
    }

    pub(crate) fn push_sequence(&mut self, rows: Vec<LineRow>, end: u32) {
        let Some(first) = rows.first() else {
            return;
        };
        if end <= first.address {
            return;
        }
        self.sequences.push(Sequence {
            start: first.address,
            end,
            rows,
        });
    }

    pub(crate) fn finish(&mut self) {
        self.sequences.sort_by_key(|s| s.start);
        for sequence in &mut self.sequences {
            sequence.rows.sort_by_key(|r| r.address);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sequences.is_empty()
    }

    pub fn files(&self) -> &[String] {
        &self.files
    }

    pub fn file_path(&self, file: FileId) -> Option<&str> {
        self.files.get(file as usize).map(String::as_str)
    }

    /// Files that are referenced by at least one row with a line number.
    pub fn files_with_code(&self) -> Vec<FileId> {
        let mut set = BTreeSet::new();
        for sequence in &self.sequences {
            for row in &sequence.rows {
                if row.line != 0 {
                    set.insert(row.file);
                }
            }
        }
        set.into_iter().collect()
    }

    fn sequence_at(&self, address: u32) -> Option<&Sequence> {
        let pos = self.sequences.partition_point(|s| s.start <= address);
        let candidate = self.sequences.get(pos.checked_sub(1)?)?;
        (address < candidate.end).then_some(candidate)
    }

    /// Row that covers the instruction at `address`.
    pub fn row_at(&self, address: u32) -> Option<&LineRow> {
        let sequence = self.sequence_at(address)?;
        let pos = sequence.rows.partition_point(|r| r.address <= address);
        sequence.rows.get(pos.checked_sub(1)?)
    }

    /// Source line of the instruction at `address`, skipping line-zero rows.
    pub fn location(&self, address: u32) -> Option<SourceLine> {
        let sequence = self.sequence_at(address)?;
        let pos = sequence.rows.partition_point(|r| r.address <= address);
        let row = sequence.rows.get(pos.checked_sub(1)?)?;
        if row.line != 0 {
            return Some(SourceLine {
                file: row.file,
                line: row.line,
                column: row.column,
            });
        }
        sequence.rows[..pos]
            .iter()
            .rev()
            .find(|r| r.line != 0)
            .map(|r| SourceLine {
                file: r.file,
                line: r.line,
                column: r.column,
            })
    }

    /// True when `address` begins a statement row with a real line number.
    pub fn is_statement_start(&self, address: u32) -> bool {
        let Some(sequence) = self.sequence_at(address) else {
            return false;
        };
        sequence
            .rows
            .binary_search_by_key(&address, |r| r.address)
            .ok()
            .map(|i| {
                // Several rows may share an address; the last one is effective.
                let mut j = i;
                while j + 1 < sequence.rows.len() && sequence.rows[j + 1].address == address {
                    j += 1;
                }
                let row = &sequence.rows[j];
                row.is_stmt && row.line != 0
            })
            .unwrap_or(false)
    }

    /// End of the prologue of the code sequence containing `address`, if marked.
    pub fn prologue_end_after(&self, address: u32) -> Option<u32> {
        let sequence = self.sequence_at(address)?;
        sequence
            .rows
            .iter()
            .find(|r| r.prologue_end && r.address >= address)
            .map(|r| r.address)
    }

    /// Files whose recorded path best matches `query` (already DWARF-mapped).
    pub fn find_files(&self, query: &str) -> Vec<FileId> {
        let wanted = paths::key(query);
        let exact: Vec<FileId> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| paths::key(f) == wanted)
            .map(|(i, _)| i as FileId)
            .collect();
        if !exact.is_empty() {
            return exact;
        }

        let scored: Vec<(FileId, usize)> = self
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                (
                    i as FileId,
                    paths::common_suffix_len(&paths::key(f), &wanted),
                )
            })
            .filter(|(_, score)| *score > 0)
            .collect();
        let Some(best) = scored.iter().map(|(_, s)| *s).max() else {
            return Vec::new();
        };
        let winners: Vec<FileId> = scored
            .iter()
            .filter(|(_, s)| *s == best)
            .map(|(f, _)| *f)
            .collect();
        // A bare file-name match is only trusted when it is unambiguous.
        if best >= 2 || winners.len() == 1 {
            winners
        } else {
            Vec::new()
        }
    }

    /// Maps a requested line to breakpoint addresses, sliding forward to the
    /// next line that has code when the exact line has none.
    pub fn resolve_line(&self, files: &[FileId], line: u32) -> Option<ResolvedLine> {
        if files.is_empty() {
            return None;
        }
        let mut by_line: BTreeMap<u32, Vec<(usize, LineRow)>> = BTreeMap::new();
        for (seq_index, sequence) in self.sequences.iter().enumerate() {
            for row in &sequence.rows {
                if row.is_stmt && row.line >= line && files.contains(&row.file) {
                    by_line.entry(row.line).or_default().push((seq_index, *row));
                }
            }
        }
        let (actual, rows) = by_line.into_iter().next()?;
        let mut per_sequence: BTreeMap<usize, LineRow> = BTreeMap::new();
        for (seq_index, row) in rows {
            per_sequence
                .entry(seq_index)
                .and_modify(|best| {
                    if row.address < best.address {
                        *best = row;
                    }
                })
                .or_insert(row);
        }
        let column = per_sequence.values().map(|r| r.column).min().unwrap_or(0);
        let mut addresses: Vec<u32> = per_sequence.values().map(|r| r.address).collect();
        addresses.sort_unstable();
        Some(ResolvedLine {
            line: actual,
            column,
            addresses,
        })
    }

    /// Distinct statement positions between two lines (inclusive).
    pub fn statement_positions(&self, files: &[FileId], start: u32, end: u32) -> Vec<(u32, u32)> {
        let mut set = BTreeSet::new();
        for sequence in &self.sequences {
            for row in &sequence.rows {
                if row.is_stmt && row.line >= start && row.line <= end && files.contains(&row.file)
                {
                    set.insert((row.line, row.column));
                }
            }
        }
        set.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(address: u32, line: u32, is_stmt: bool) -> LineRow {
        LineRow {
            address,
            file: 0,
            line,
            column: 1,
            is_stmt,
            prologue_end: false,
        }
    }

    fn table() -> LineTable {
        let mut t = LineTable::default();
        let f = t.intern_file("/src/main.rs".into());
        assert_eq!(f, 0);
        t.push_sequence(
            vec![
                row(100, 3, true),
                row(110, 4, true),
                row(120, 0, false),
                row(125, 4, true),
                row(130, 7, true),
            ],
            140,
        );
        t.push_sequence(vec![row(200, 10, true), row(210, 4, true)], 220);
        t.finish();
        t
    }

    #[test]
    fn location_skips_line_zero_rows() {
        let t = table();
        assert_eq!(t.location(105).unwrap().line, 3);
        assert_eq!(t.location(121).unwrap().line, 4);
        assert!(t.location(140).is_none());
        assert!(t.location(50).is_none());
    }

    #[test]
    fn statement_start_requires_exact_address() {
        let t = table();
        assert!(t.is_statement_start(110));
        assert!(!t.is_statement_start(111));
        assert!(!t.is_statement_start(120));
    }

    #[test]
    fn resolve_line_picks_lowest_address_per_sequence() {
        let t = table();
        let resolved = t.resolve_line(&[0], 4).unwrap();
        assert_eq!(resolved.line, 4);
        assert_eq!(resolved.addresses, vec![110, 210]);
    }

    #[test]
    fn resolve_line_slides_to_next_line_with_code() {
        let t = table();
        let resolved = t.resolve_line(&[0], 5).unwrap();
        assert_eq!(resolved.line, 7);
        assert_eq!(resolved.addresses, vec![130]);
        assert!(t.resolve_line(&[0], 11).is_none());
    }

    #[test]
    fn find_files_prefers_exact_then_unique_suffix() {
        let mut t = table();
        t.intern_file("/other/src/lib.rs".into());
        assert_eq!(t.find_files("/src/main.rs"), vec![0]);
        assert_eq!(t.find_files("C:/checkout/src/main.rs"), vec![0]);
        assert_eq!(t.find_files("lib.rs"), vec![1]);
        assert!(t.find_files("nothing.rs").is_empty());
    }
}
