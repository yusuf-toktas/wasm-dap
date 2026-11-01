use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::dwarf::{DebugInfo, SourceLine, SourceMap};
use crate::error::Result;
use crate::module::{Disassembly, WasmModule};

/// Where a code location is shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeSource {
    /// A source file described by DWARF, already mapped to the local file system.
    File {
        path: String,
        line: u32,
        column: u32,
    },
    /// A line of the module's text-format disassembly.
    Wat { line: u32 },
}

/// Immutable program information shared by the adapter and the debuggee thread.
pub struct ProgramInfo {
    pub path: PathBuf,
    pub module: Arc<WasmModule>,
    pub debug: Option<Arc<DebugInfo>>,
    pub source_map: SourceMap,
    instructions: OnceLock<Vec<(u32, u32)>>,
}

impl std::fmt::Debug for ProgramInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramInfo")
            .field("path", &self.path)
            .field("debug", &self.debug.is_some())
            .finish()
    }
}

impl ProgramInfo {
    pub fn new(
        path: &Path,
        module: Arc<WasmModule>,
        debug: Option<Arc<DebugInfo>>,
        source_map: SourceMap,
    ) -> Self {
        Self {
            path: path.to_path_buf(),
            module,
            debug,
            source_map,
            instructions: OnceLock::new(),
        }
    }

    pub fn module_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "module.wasm".to_string())
    }

    /// Name of the virtual text-format document.
    pub fn wat_name(&self) -> String {
        let stem = self
            .path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "module".to_string());
        format!("{stem}.wat")
    }

    pub fn disassembly(&self) -> Result<Arc<Disassembly>> {
        self.module.disassembly()
    }

    /// DWARF line of an instruction, with the file mapped to a local path.
    pub fn dwarf_line(&self, pc: u32) -> Option<(SourceLine, String)> {
        let debug = self.debug.as_ref()?;
        let line = debug.lines().location(pc)?;
        let path = debug.lines().file_path(line.file)?;
        Some((line, self.source_map.to_local(path)))
    }

    /// Source position of an instruction, preferring DWARF over the WAT view.
    pub fn code_source(&self, pc: u32) -> Option<CodeSource> {
        if let Some((line, path)) = self.dwarf_line(pc) {
            return Some(CodeSource::File {
                path,
                line: line.line,
                column: line.column,
            });
        }
        let disassembly = self.disassembly().ok()?;
        disassembly
            .line_of(pc)
            .or_else(|| disassembly.line_at_or_before(pc))
            .map(|l| CodeSource::Wat { line: l + 1 })
    }

    /// Display name of the function containing `pc`.
    pub fn function_name_at(&self, pc: u32) -> Option<String> {
        if let Some(subprogram) = self.debug.as_ref().and_then(|d| d.subprogram_at(pc)) {
            return Some(subprogram.name.clone());
        }
        let function = self.module.function_at(pc)?;
        Some(self.module.display_function_name(function.index))
    }

    /// True when DWARF line information covers the function containing `pc`.
    pub fn has_line_info(&self, pc: u32) -> bool {
        let Some(debug) = &self.debug else {
            return false;
        };
        if debug.lines().location(pc).is_some() {
            return true;
        }
        self.module
            .function_at(pc)
            .and_then(|f| f.body.as_ref())
            .and_then(|b| b.first_instruction())
            .is_some_and(|first| debug.lines().location(first).is_some())
    }

    /// All instruction offsets in code order, paired with their function index.
    pub fn instructions(&self) -> &[(u32, u32)] {
        self.instructions.get_or_init(|| {
            let mut all = Vec::new();
            for function in self.module.defined_functions() {
                if let Some(body) = &function.body {
                    all.extend(body.instructions.iter().map(|i| (i.offset, function.index)));
                }
            }
            all.sort_unstable();
            all
        })
    }
}
