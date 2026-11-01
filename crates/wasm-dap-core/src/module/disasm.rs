use crate::error::{Error, Result};

use super::WasmModule;

/// WebAssembly text-format rendering of a module with binary offset mappings.
///
/// The text doubles as a virtual source document for modules without DWARF
/// and as the instruction listing for the DAP `disassemble` request.
#[derive(Debug)]
pub struct Disassembly {
    lines: Vec<String>,
    /// Instruction offset to zero-based line index, sorted by offset.
    by_offset: Vec<(u32, u32)>,
    /// Zero-based line index to the first instruction offset printed on it.
    by_line: Vec<Option<u32>>,
    /// Offsets of each function's final `end`, which the text format renders
    /// as a closing parenthesis (or not at all for empty functions).
    final_ends: Vec<u32>,
}

impl Disassembly {
    pub(crate) fn new(module: &WasmModule) -> Result<Self> {
        let mut config = wasmprinter::Config::new();
        config.fold_instructions(false);
        config.print_offsets(false);
        let mut storage = String::new();
        let mut lines = Vec::new();
        let mut line_offsets = Vec::new();
        for (offset, line) in config
            .offsets_and_lines(module.bytes(), &mut storage)
            .map_err(|e| Error::InvalidModule(format!("failed to disassemble module: {e}")))?
        {
            lines.push(line.trim_end_matches(['\r', '\n']).to_string());
            line_offsets.push(offset.map(|o| o as u32));
        }

        let mut printed: Vec<(u32, u32)> = line_offsets
            .iter()
            .enumerate()
            .filter_map(|(line, offset)| offset.map(|o| (o, line as u32)))
            .collect();
        printed.sort_unstable();
        printed.dedup_by_key(|(offset, _)| *offset);

        let mut by_offset = Vec::new();
        let mut by_line = vec![None; lines.len()];
        let mut final_ends = Vec::new();
        for function in module.defined_functions() {
            let Some(body) = function.body.as_ref() else {
                continue;
            };
            final_ends.extend(body.last_instruction());
            let mut last_line = None;
            for instruction in &body.instructions {
                let exact = printed
                    .binary_search_by_key(&instruction.offset, |(o, _)| *o)
                    .ok()
                    .map(|i| printed[i].1);
                let preceding = || {
                    let pos = printed.partition_point(|(o, _)| *o <= instruction.offset);
                    pos.checked_sub(1).map(|i| printed[i].1)
                };
                let Some(line) = exact.or(last_line).or_else(preceding) else {
                    continue;
                };
                last_line = Some(line);
                by_offset.push((instruction.offset, line));
                let slot = &mut by_line[line as usize];
                if slot.is_none() {
                    *slot = Some(instruction.offset);
                }
            }
        }
        by_offset.sort_unstable();
        final_ends.sort_unstable();

        Ok(Self {
            lines,
            by_offset,
            by_line,
            final_ends,
        })
    }

    pub fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Zero-based line on which the instruction at `offset` is printed.
    pub fn line_of(&self, offset: u32) -> Option<u32> {
        self.by_offset
            .binary_search_by_key(&offset, |(o, _)| *o)
            .ok()
            .map(|i| self.by_offset[i].1)
    }

    /// Zero-based line of the closest instruction at or before `offset`.
    pub fn line_at_or_before(&self, offset: u32) -> Option<u32> {
        let pos = self.by_offset.partition_point(|(o, _)| *o <= offset);
        self.by_offset
            .get(pos.checked_sub(1)?)
            .map(|(_, line)| *line)
    }

    /// First instruction offset printed on the given zero-based line.
    pub fn offset_of_line(&self, line: u32) -> Option<u32> {
        self.by_line.get(line as usize).copied().flatten()
    }

    /// Nearest line at or after `line` (within `limit` lines) that holds an instruction.
    pub fn next_instruction_line(&self, line: u32, limit: u32) -> Option<(u32, u32)> {
        let end = (line.saturating_add(limit) as usize).min(self.by_line.len());
        (line as usize..end).find_map(|l| self.by_line[l].map(|offset| (l as u32, offset)))
    }

    /// Text of the instruction at `offset`, without indentation.
    pub fn instruction_text(&self, offset: u32) -> Option<&str> {
        if self.final_ends.binary_search(&offset).is_ok() {
            return Some("end");
        }
        let line = self.line_of(offset)?;
        Some(self.lines[line as usize].trim())
    }

    pub fn line(&self, line: u32) -> Option<&str> {
        self.lines.get(line as usize).map(String::as_str)
    }
}
