//! DWARF debug information embedded in WebAssembly custom sections.
//!
//! WebAssembly DWARF addresses are offsets relative to the start of the code
//! section payload. This module converts them to module offsets, which is the
//! address space used by Wasmtime's debugging API and by the disassembler.

pub mod expr;
pub mod lines;
pub mod paths;
pub mod types;
pub mod vars;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use gimli::Reader as _;
use gimli::{AttributeValue, DebuggingInformationEntry, UnitOffset};
use wasmparser::{Parser, Payload};

pub use expr::{ExprContext, Location, WasmStorage};
pub use lines::{FileId, LineRow, LineTable, ResolvedLine, SourceLine};
pub use paths::SourceMap;
pub use types::{BaseEncoding, Member, TypeDef, TypeId, TypeKind, Variant, VariantPart};
pub use vars::{FrameVariables, Variable, VariableLocation};

use crate::error::Result;

pub type Reader = gimli::EndianArcSlice<gimli::LittleEndian>;

pub(crate) struct UnitEntry {
    pub unit: gimli::Unit<Reader>,
    start: usize,
    end: usize,
    pub rust: bool,
}

/// A function with code, as described by a `DW_TAG_subprogram`.
#[derive(Debug, Clone)]
pub struct Subprogram {
    /// Name qualified with enclosing namespaces (`crate::module::function`).
    pub name: String,
    pub low: u32,
    pub high: u32,
    pub unit: usize,
    pub offset: usize,
    pub decl_line: Option<u32>,
}

/// Parsed DWARF information for one module.
pub struct DebugInfo {
    dwarf: gimli::Dwarf<Reader>,
    units: Vec<UnitEntry>,
    code_offset: u32,
    code_end: u32,
    address_size: u8,
    lines: LineTable,
    subprograms: Vec<Subprogram>,
    producers: Vec<String>,
    type_cache: Mutex<HashMap<TypeId, Arc<TypeDef>>>,
    global_index: OnceLock<HashMap<String, Vec<(usize, usize)>>>,
}

impl std::fmt::Debug for DebugInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugInfo")
            .field("units", &self.units.len())
            .field("files", &self.lines.files().len())
            .field("subprograms", &self.subprograms.len())
            .finish()
    }
}

impl DebugInfo {
    /// Loads DWARF from a module binary. Returns `Ok(None)` when the module has
    /// no `.debug_info`/`.debug_line` sections.
    pub fn load(module_bytes: &[u8]) -> Result<Option<Self>> {
        Self::load_with_bias(module_bytes, 0)
    }

    /// Like [`DebugInfo::load`], adding `bias` to every DWARF code address.
    pub fn load_with_bias(module_bytes: &[u8], bias: u32) -> Result<Option<Self>> {
        let mut sections: HashMap<String, Arc<[u8]>> = HashMap::new();
        let mut code: Option<(u32, u32)> = None;
        for payload in Parser::new(0).parse_all(module_bytes) {
            match payload? {
                Payload::CodeSectionStart { range, .. } => {
                    code = Some((range.start as u32, range.end as u32));
                }
                Payload::CustomSection(reader) if reader.name().starts_with(".debug_") => {
                    sections.insert(reader.name().to_string(), Arc::from(reader.data()));
                }
                _ => {}
            }
        }
        if !sections.contains_key(".debug_info") {
            return Ok(None);
        }
        let Some((code_start, code_end)) = code else {
            return Ok(None);
        };
        let code_offset = code_start + bias;

        let empty: Arc<[u8]> = Arc::from(&[][..]);
        let dwarf = gimli::Dwarf::load(|id| -> std::result::Result<Reader, gimli::Error> {
            let data = sections
                .get(id.name())
                .cloned()
                .unwrap_or_else(|| empty.clone());
            Ok(Reader::new(data, gimli::LittleEndian))
        })?;

        let mut info = DebugInfo {
            dwarf,
            units: Vec::new(),
            code_offset,
            code_end,
            address_size: 4,
            lines: LineTable::default(),
            subprograms: Vec::new(),
            producers: Vec::new(),
            type_cache: Mutex::new(HashMap::new()),
            global_index: OnceLock::new(),
        };
        info.load_units()?;
        info.load_lines();
        info.load_subprograms();
        if info.lines.is_empty() && info.subprograms.is_empty() {
            return Ok(None);
        }
        Ok(Some(info))
    }

    fn load_units(&mut self) -> Result<()> {
        let mut headers = self.dwarf.units();
        while let Some(header) = headers.next()? {
            let start = header.offset().0;
            let end = start + header.length_including_self();
            let unit = match self.dwarf.unit(header) {
                Ok(unit) => unit,
                Err(e) => {
                    tracing::warn!("skipping unparsable DWARF unit at {start:#x}: {e}");
                    continue;
                }
            };
            self.address_size = unit.encoding().address_size;
            let mut rust = false;
            if let Ok(root) = unit.entry(unit.header.root_offset()) {
                if let Some(AttributeValue::Language(lang)) = root.attr_value(gimli::DW_AT_language)
                {
                    rust = lang == gimli::DW_LANG_Rust;
                }
                if let Some(producer) = root
                    .attr_value(gimli::DW_AT_producer)
                    .and_then(|v| self.dwarf.attr_string(&unit, v).ok())
                    .map(|s| {
                        s.to_string_lossy()
                            .map(|c| c.into_owned())
                            .unwrap_or_default()
                    })
                    && !producer.is_empty()
                    && !self.producers.contains(&producer)
                {
                    self.producers.push(producer);
                }
            }
            self.units.push(UnitEntry {
                unit,
                start,
                end,
                rust,
            });
        }
        Ok(())
    }

    fn load_lines(&mut self) {
        let mut lines = LineTable::default();
        for entry in &self.units {
            let unit = &entry.unit;
            let Some(program) = unit.line_program.clone() else {
                continue;
            };
            let mut file_ids: HashMap<u64, FileId> = HashMap::new();
            let mut rows = program.rows();
            let mut current: Vec<LineRow> = Vec::new();
            let mut valid = true;
            loop {
                let (header, row) = match rows.next_row() {
                    Ok(Some(next)) => next,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("malformed DWARF line program: {e}");
                        break;
                    }
                };
                let address = self.to_pc(row.address());
                if row.end_sequence() {
                    if valid && let Some(end) = address {
                        lines.push_sequence(std::mem::take(&mut current), end);
                    }
                    current.clear();
                    valid = true;
                    continue;
                }
                let Some(address) = address else {
                    valid = false;
                    continue;
                };
                if !valid {
                    continue;
                }
                let file_index = row.file_index();
                let file = match file_ids.get(&file_index) {
                    Some(id) => *id,
                    None => {
                        let path = row
                            .file(header)
                            .map(|f| self.file_entry_path(unit, header, f))
                            .unwrap_or_default();
                        let id = lines.intern_file(path);
                        file_ids.insert(file_index, id);
                        id
                    }
                };
                current.push(LineRow {
                    address,
                    file,
                    line: row.line().map(|l| l.get() as u32).unwrap_or(0),
                    column: match row.column() {
                        gimli::ColumnType::LeftEdge => 0,
                        gimli::ColumnType::Column(c) => c.get() as u32,
                    },
                    is_stmt: row.is_stmt(),
                    prologue_end: row.prologue_end(),
                });
            }
        }
        lines.finish();
        self.lines = lines;
    }

    fn file_entry_path(
        &self,
        unit: &gimli::Unit<Reader>,
        header: &gimli::LineProgramHeader<Reader>,
        file: &gimli::FileEntry<Reader>,
    ) -> String {
        let string = |value: AttributeValue<Reader>| {
            self.dwarf
                .attr_string(unit, value)
                .ok()
                .and_then(|s| s.to_string_lossy().ok().map(|c| c.into_owned()))
        };
        let name = string(file.path_name()).unwrap_or_default();
        let dir = file.directory(header).and_then(string);
        let comp_dir = unit
            .comp_dir
            .as_ref()
            .and_then(|c| c.to_string_lossy().ok().map(|c| c.into_owned()));
        paths::join(comp_dir.as_deref(), dir.as_deref(), &name)
    }

    fn load_subprograms(&mut self) {
        let mut subprograms = Vec::new();
        for (unit_index, entry) in self.units.iter().enumerate() {
            let unit = &entry.unit;
            let mut cursor = unit.entries();
            let mut scopes: Vec<(isize, String)> = Vec::new();
            loop {
                let die = match cursor.next_dfs() {
                    Ok(Some(die)) => die.clone(),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("malformed DWARF DIE tree: {e}");
                        break;
                    }
                };
                let depth = die.depth();
                while scopes.last().is_some_and(|(d, _)| *d >= depth) {
                    scopes.pop();
                }
                match die.tag() {
                    gimli::DW_TAG_namespace
                    | gimli::DW_TAG_structure_type
                    | gimli::DW_TAG_class_type
                    | gimli::DW_TAG_union_type
                    | gimli::DW_TAG_enumeration_type => {
                        if let Some(name) = self.die_name(unit_index, &die) {
                            scopes.push((depth, name));
                        }
                    }
                    gimli::DW_TAG_subprogram => {
                        self.push_subprogram(unit_index, &die, &scopes, &mut subprograms);
                    }
                    _ => {}
                }
            }
        }
        subprograms.sort_by_key(|s| (s.low, std::cmp::Reverse(s.high)));
        self.subprograms = subprograms;
    }

    fn push_subprogram(
        &self,
        unit_index: usize,
        die: &DebuggingInformationEntry<Reader>,
        scopes: &[(isize, String)],
        out: &mut Vec<Subprogram>,
    ) {
        if die.attr_value(gimli::DW_AT_declaration).is_some() {
            return;
        }
        let unit = &self.units[unit_index].unit;
        let origin = self.abstract_origin(unit_index, die);
        let Some(name) = self
            .die_name(unit_index, die)
            .or_else(|| origin.as_ref().and_then(|(u, o)| self.die_name(*u, o)))
        else {
            return;
        };
        let qualified = if scopes.is_empty() {
            name
        } else {
            let mut parts: Vec<&str> = scopes.iter().map(|(_, n)| n.as_str()).collect();
            parts.push(&name);
            parts.join("::")
        };
        let decl_line = die
            .attr_value(gimli::DW_AT_decl_line)
            .or_else(|| {
                origin
                    .as_ref()
                    .and_then(|(_, o)| o.attr_value(gimli::DW_AT_decl_line))
            })
            .and_then(|v| v.udata_value())
            .map(|l| l as u32);
        let Ok(mut ranges) = self.dwarf.die_ranges(unit, die) else {
            return;
        };
        while let Ok(Some(range)) = ranges.next() {
            let (Some(low), Some(high)) = (self.to_pc(range.begin), self.to_pc(range.end)) else {
                continue;
            };
            if high <= low {
                continue;
            }
            out.push(Subprogram {
                name: qualified.clone(),
                low,
                high,
                unit: unit_index,
                offset: die.offset().0,
                decl_line,
            });
        }
    }

    /// Converts a DWARF code address to a module offset, rejecting tombstones.
    pub(crate) fn to_pc(&self, address: u64) -> Option<u32> {
        if address == 0 || address >= 0xffff_fff0 {
            return None;
        }
        let pc = u64::from(self.code_offset).checked_add(address)?;
        let pc = u32::try_from(pc).ok()?;
        (pc <= self.code_end).then_some(pc)
    }

    pub fn lines(&self) -> &LineTable {
        &self.lines
    }

    pub fn subprograms(&self) -> &[Subprogram] {
        &self.subprograms
    }

    pub fn producers(&self) -> &[String] {
        &self.producers
    }

    pub fn unit_count(&self) -> usize {
        self.units.len()
    }

    pub fn address_size(&self) -> u8 {
        self.address_size
    }

    /// Innermost subprogram whose code range contains `pc`.
    pub fn subprogram_at(&self, pc: u32) -> Option<&Subprogram> {
        let end = self.subprograms.partition_point(|s| s.low <= pc);
        self.subprograms[..end]
            .iter()
            .rev()
            .take(64)
            .filter(|s| pc < s.high)
            .min_by_key(|s| s.high - s.low)
    }

    /// Subprograms whose name or qualified name matches `query`.
    pub fn find_subprograms(&self, query: &str) -> Vec<&Subprogram> {
        let query = query.trim();
        self.subprograms
            .iter()
            .filter(|s| {
                s.name == query
                    || s.name
                        .rsplit_once("::")
                        .is_some_and(|(_, last)| last == query)
                    || s.name.ends_with(&format!("::{query}"))
            })
            .collect()
    }

    pub(crate) fn die_name(
        &self,
        unit_index: usize,
        entry: &DebuggingInformationEntry<Reader>,
    ) -> Option<String> {
        let value = entry.attr_value(gimli::DW_AT_name)?;
        let unit = &self.units.get(unit_index)?.unit;
        let text = self.dwarf.attr_string(unit, value).ok()?;
        text.to_string_lossy().ok().map(|c| c.into_owned())
    }

    pub(crate) fn die_type(
        &self,
        unit_index: usize,
        entry: &DebuggingInformationEntry<Reader>,
    ) -> Option<TypeId> {
        let value = entry.attr_value(gimli::DW_AT_type)?;
        let (unit, offset) = self.reference(unit_index, value)?;
        Some(TypeId { unit, offset })
    }

    /// Resolves a DIE reference attribute to (unit index, unit offset).
    pub(crate) fn reference(
        &self,
        unit_index: usize,
        value: AttributeValue<Reader>,
    ) -> Option<(usize, usize)> {
        match value {
            AttributeValue::UnitRef(offset) => Some((unit_index, offset.0)),
            AttributeValue::DebugInfoRef(offset) => {
                let target = offset.0;
                let index = self
                    .units
                    .iter()
                    .position(|u| u.start <= target && target < u.end)?;
                let unit = &self.units[index].unit;
                let unit_offset: UnitOffset = offset.to_unit_offset(&unit.header)?;
                Some((index, unit_offset.0))
            }
            _ => None,
        }
    }
}
