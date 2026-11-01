//! Static analysis of WebAssembly module binaries.

mod disasm;
mod names;

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use wasmparser::{
    CompositeInnerType, ConstExpr, DataKind, ExternalKind, KnownCustom, Operator, Parser, Payload,
    TypeRef,
};

pub use disasm::Disassembly;
pub use names::{Names, demangle};

use crate::error::{Error, Result};

/// A WebAssembly value type in display-friendly form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueType {
    I32,
    I64,
    F32,
    F64,
    V128,
    Ref(String),
}

impl ValueType {
    /// Size in bytes of the value when stored in linear memory.
    pub fn byte_size(&self) -> usize {
        match self {
            ValueType::I32 | ValueType::F32 => 4,
            ValueType::I64 | ValueType::F64 => 8,
            ValueType::V128 => 16,
            ValueType::Ref(_) => 4,
        }
    }
}

impl From<wasmparser::ValType> for ValueType {
    fn from(ty: wasmparser::ValType) -> Self {
        match ty {
            wasmparser::ValType::I32 => ValueType::I32,
            wasmparser::ValType::I64 => ValueType::I64,
            wasmparser::ValType::F32 => ValueType::F32,
            wasmparser::ValType::F64 => ValueType::F64,
            wasmparser::ValType::V128 => ValueType::V128,
            wasmparser::ValType::Ref(r) => ValueType::Ref(r.to_string()),
        }
    }
}

impl fmt::Display for ValueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueType::I32 => f.write_str("i32"),
            ValueType::I64 => f.write_str("i64"),
            ValueType::F32 => f.write_str("f32"),
            ValueType::F64 => f.write_str("f64"),
            ValueType::V128 => f.write_str("v128"),
            ValueType::Ref(r) => f.write_str(r),
        }
    }
}

/// A function signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncSig {
    pub params: Vec<ValueType>,
    pub results: Vec<ValueType>,
}

impl fmt::Display for FuncSig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let params: Vec<String> = self.params.iter().map(ToString::to_string).collect();
        write!(f, "({})", params.join(", "))?;
        match self.results.len() {
            0 => Ok(()),
            1 => write!(f, " -> {}", self.results[0]),
            _ => {
                let results: Vec<String> = self.results.iter().map(ToString::to_string).collect();
                write!(f, " -> ({})", results.join(", "))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExternKind {
    Func,
    Table,
    Memory,
    Global,
    Tag,
}

impl ExternKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ExternKind::Func => "func",
            ExternKind::Table => "table",
            ExternKind::Memory => "memory",
            ExternKind::Global => "global",
            ExternKind::Tag => "tag",
        }
    }
}

impl fmt::Display for ExternKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub kind: ExternKind,
    /// Index within the item's own index space (function, memory, ...).
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub kind: ExternKind,
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryInfo {
    pub initial: u64,
    pub maximum: Option<u64>,
    pub memory64: bool,
    pub shared: bool,
    pub page_size: u64,
    pub imported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub element: String,
    pub initial: u64,
    pub maximum: Option<u64>,
    pub table64: bool,
    pub imported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalInfo {
    pub ty: ValueType,
    pub mutable: bool,
    pub imported: bool,
    /// Human-readable initializer, e.g. `i32.const 1048576`.
    pub init: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSegment {
    pub size: usize,
    pub memory: Option<u32>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomSection {
    pub name: String,
    pub offset: usize,
    pub size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerField {
    pub name: String,
    pub values: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrKind {
    Other,
    Call,
    TailCall,
    Return,
    /// A `loop` header; branches back to the loop skip this instruction.
    Loop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instruction {
    /// Offset of the opcode within the module binary.
    pub offset: u32,
    pub kind: InstrKind,
}

#[derive(Debug, Clone)]
pub struct FunctionBody {
    /// Range of the body (local declarations and code) within the module binary.
    pub range: Range<u32>,
    pub locals: Vec<(u32, ValueType)>,
    pub instructions: Vec<Instruction>,
}

impl FunctionBody {
    pub fn first_instruction(&self) -> Option<u32> {
        self.instructions.first().map(|i| i.offset)
    }

    /// Offset of the function's final `end` opcode.
    pub fn last_instruction(&self) -> Option<u32> {
        self.instructions.last().map(|i| i.offset)
    }

    pub fn instruction_index(&self, offset: u32) -> Option<usize> {
        self.instructions
            .binary_search_by_key(&offset, |i| i.offset)
            .ok()
    }

    pub fn local_count(&self) -> u64 {
        self.locals.iter().map(|(n, _)| u64::from(*n)).sum()
    }

    /// Type of a declared (non-parameter) local, counting from zero.
    pub fn declared_local_type(&self, mut index: u32) -> Option<&ValueType> {
        for (count, ty) in &self.locals {
            if index < *count {
                return Some(ty);
            }
            index -= count;
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct Function {
    /// Index in the module's function index space (imports first).
    pub index: u32,
    pub type_index: u32,
    pub import: Option<(String, String)>,
    pub body: Option<FunctionBody>,
}

/// Parsed static information about a WebAssembly module.
#[derive(Debug)]
pub struct WasmModule {
    bytes: Arc<[u8]>,
    pub types: Vec<Option<FuncSig>>,
    pub imports: Vec<Import>,
    pub functions: Vec<Function>,
    pub tables: Vec<TableInfo>,
    pub memories: Vec<MemoryInfo>,
    pub globals: Vec<GlobalInfo>,
    pub tags: u32,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub data_segments: Vec<DataSegment>,
    pub element_segments: u32,
    pub custom_sections: Vec<CustomSection>,
    /// Range of the code section payload; DWARF addresses are relative to its start.
    pub code_section: Option<Range<u32>>,
    pub names: Names,
    pub producers: Vec<ProducerField>,
    defined_by_offset: Vec<u32>,
    disassembly: OnceLock<std::result::Result<Arc<Disassembly>, String>>,
}

impl WasmModule {
    pub fn parse(bytes: impl Into<Arc<[u8]>>) -> Result<Self> {
        let bytes: Arc<[u8]> = bytes.into();
        let mut module = WasmModule {
            bytes: bytes.clone(),
            types: Vec::new(),
            imports: Vec::new(),
            functions: Vec::new(),
            tables: Vec::new(),
            memories: Vec::new(),
            globals: Vec::new(),
            tags: 0,
            exports: Vec::new(),
            start: None,
            data_segments: Vec::new(),
            element_segments: 0,
            custom_sections: Vec::new(),
            code_section: None,
            names: Names::default(),
            producers: Vec::new(),
            defined_by_offset: Vec::new(),
            disassembly: OnceLock::new(),
        };
        module.read(&bytes)?;
        Ok(module)
    }

    fn read(&mut self, bytes: &[u8]) -> Result<()> {
        let mut next_body = 0usize;
        let mut first_defined = 0usize;

        for payload in Parser::new(0).parse_all(bytes) {
            match payload? {
                Payload::Version { encoding, .. } => {
                    if encoding != wasmparser::Encoding::Module {
                        return Err(Error::Unsupported(
                            "WebAssembly components are not supported; provide a core module"
                                .into(),
                        ));
                    }
                }
                Payload::TypeSection(reader) => {
                    for group in reader {
                        for sub in group?.into_types() {
                            self.types.push(match sub.composite_type.inner {
                                CompositeInnerType::Func(ft) => Some(FuncSig {
                                    params: ft.params().iter().map(|t| (*t).into()).collect(),
                                    results: ft.results().iter().map(|t| (*t).into()).collect(),
                                }),
                                _ => None,
                            });
                        }
                    }
                }
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        let import = import?;
                        let (kind, index) = match import.ty {
                            TypeRef::Func(type_index) | TypeRef::FuncExact(type_index) => {
                                let index = self.functions.len() as u32;
                                self.functions.push(Function {
                                    index,
                                    type_index,
                                    import: Some((
                                        import.module.to_string(),
                                        import.name.to_string(),
                                    )),
                                    body: None,
                                });
                                (ExternKind::Func, index)
                            }
                            TypeRef::Table(t) => {
                                self.tables.push(table_info(&t, true));
                                (ExternKind::Table, self.tables.len() as u32 - 1)
                            }
                            TypeRef::Memory(m) => {
                                self.memories.push(memory_info(&m, true));
                                (ExternKind::Memory, self.memories.len() as u32 - 1)
                            }
                            TypeRef::Global(g) => {
                                self.globals.push(GlobalInfo {
                                    ty: g.content_type.into(),
                                    mutable: g.mutable,
                                    imported: true,
                                    init: None,
                                });
                                (ExternKind::Global, self.globals.len() as u32 - 1)
                            }
                            TypeRef::Tag(_) => {
                                self.tags += 1;
                                (ExternKind::Tag, self.tags - 1)
                            }
                        };
                        self.imports.push(Import {
                            module: import.module.to_string(),
                            name: import.name.to_string(),
                            kind,
                            index,
                        });
                    }
                    first_defined = self.functions.len();
                    next_body = first_defined;
                }
                Payload::FunctionSection(reader) => {
                    first_defined = self.functions.len();
                    next_body = first_defined;
                    for type_index in reader {
                        let index = self.functions.len() as u32;
                        self.functions.push(Function {
                            index,
                            type_index: type_index?,
                            import: None,
                            body: None,
                        });
                    }
                }
                Payload::TableSection(reader) => {
                    for table in reader {
                        self.tables.push(table_info(&table?.ty, false));
                    }
                }
                Payload::MemorySection(reader) => {
                    for memory in reader {
                        self.memories.push(memory_info(&memory?, false));
                    }
                }
                Payload::TagSection(reader) => {
                    self.tags += reader.count();
                }
                Payload::GlobalSection(reader) => {
                    for global in reader {
                        let global = global?;
                        self.globals.push(GlobalInfo {
                            ty: global.ty.content_type.into(),
                            mutable: global.ty.mutable,
                            imported: false,
                            init: describe_const_expr(&global.init_expr),
                        });
                    }
                }
                Payload::ExportSection(reader) => {
                    for export in reader {
                        let export = export?;
                        let kind = match export.kind {
                            ExternalKind::Func | ExternalKind::FuncExact => ExternKind::Func,
                            ExternalKind::Table => ExternKind::Table,
                            ExternalKind::Memory => ExternKind::Memory,
                            ExternalKind::Global => ExternKind::Global,
                            ExternalKind::Tag => ExternKind::Tag,
                        };
                        self.exports.push(Export {
                            name: export.name.to_string(),
                            kind,
                            index: export.index,
                        });
                    }
                }
                Payload::StartSection { func, .. } => self.start = Some(func),
                Payload::ElementSection(reader) => self.element_segments = reader.count(),
                Payload::DataSection(reader) => {
                    for data in reader {
                        let data = data?;
                        let (memory, offset) = match data.kind {
                            DataKind::Active {
                                memory_index,
                                offset_expr,
                            } => (Some(memory_index), const_expr_value(&offset_expr)),
                            DataKind::Passive => (None, None),
                        };
                        self.data_segments.push(DataSegment {
                            size: data.data.len(),
                            memory,
                            offset,
                        });
                    }
                }
                Payload::CodeSectionStart { range, .. } => {
                    self.code_section = Some(range.start as u32..range.end as u32);
                }
                Payload::CodeSectionEntry(body) => {
                    let function = self.functions.get_mut(next_body).ok_or_else(|| {
                        Error::InvalidModule("code section entry without a function".into())
                    })?;
                    function.body = Some(read_body(&body)?);
                    next_body += 1;
                }
                Payload::CustomSection(reader) => {
                    self.custom_sections.push(CustomSection {
                        name: reader.name().to_string(),
                        offset: reader.data_offset(),
                        size: reader.data().len(),
                    });
                    match reader.as_known() {
                        KnownCustom::Name(names) => self.names.read(names),
                        KnownCustom::Producers(producers) => {
                            for field in producers.into_iter().flatten() {
                                let values = field
                                    .values
                                    .into_iter()
                                    .flatten()
                                    .map(|v| (v.name.to_string(), v.version.to_string()))
                                    .collect();
                                self.producers.push(ProducerField {
                                    name: field.name.to_string(),
                                    values,
                                });
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        let mut defined: Vec<u32> = self.functions[first_defined.min(self.functions.len())..]
            .iter()
            .filter(|f| f.body.is_some())
            .map(|f| f.index)
            .collect();
        defined.sort_by_key(|i| {
            self.functions[*i as usize]
                .body
                .as_ref()
                .map(|b| b.range.start)
        });
        self.defined_by_offset = defined;
        Ok(())
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn shared_bytes(&self) -> Arc<[u8]> {
        self.bytes.clone()
    }

    pub fn imported_function_count(&self) -> u32 {
        self.functions.iter().filter(|f| f.import.is_some()).count() as u32
    }

    pub fn function(&self, index: u32) -> Option<&Function> {
        self.functions.get(index as usize)
    }

    pub fn func_sig(&self, index: u32) -> Option<&FuncSig> {
        let function = self.function(index)?;
        self.types.get(function.type_index as usize)?.as_ref()
    }

    /// Defined function whose body contains the given module offset.
    pub fn function_at(&self, offset: u32) -> Option<&Function> {
        let pos = self.defined_by_offset.partition_point(|i| {
            self.functions[*i as usize]
                .body
                .as_ref()
                .is_some_and(|b| b.range.start <= offset)
        });
        let index = *self.defined_by_offset.get(pos.checked_sub(1)?)?;
        let function = &self.functions[index as usize];
        let body = function.body.as_ref()?;
        (offset < body.range.end).then_some(function)
    }

    /// Defined functions ordered by their position in the code section.
    pub fn defined_functions(&self) -> impl Iterator<Item = &Function> {
        self.defined_by_offset
            .iter()
            .map(|i| &self.functions[*i as usize])
    }

    pub fn is_instruction(&self, offset: u32) -> bool {
        self.function_at(offset)
            .and_then(|f| f.body.as_ref())
            .and_then(|b| b.instruction_index(offset))
            .is_some()
    }

    /// Offset of the instruction that follows `offset` in the same function.
    pub fn next_instruction(&self, offset: u32) -> Option<u32> {
        let body = self.function_at(offset)?.body.as_ref()?;
        let pos = body.instructions.partition_point(|i| i.offset <= offset);
        body.instructions.get(pos).map(|i| i.offset)
    }

    /// Kind of the instruction at `offset`, if it is an opcode boundary.
    pub fn instruction_kind(&self, offset: u32) -> Option<InstrKind> {
        let body = self.function_at(offset)?.body.as_ref()?;
        let index = body.instruction_index(offset)?;
        Some(body.instructions[index].kind)
    }

    /// Raw function name from the name section, if any.
    pub fn raw_function_name(&self, index: u32) -> Option<&str> {
        self.names.functions.get(&index).map(String::as_str)
    }

    /// Human-readable function name: demangled name section entry, export or import name.
    pub fn function_name(&self, index: u32) -> Option<String> {
        if let Some(name) = self.raw_function_name(index) {
            return Some(demangle(name));
        }
        if let Some(export) = self
            .exports
            .iter()
            .find(|e| e.kind == ExternKind::Func && e.index == index)
        {
            return Some(export.name.clone());
        }
        self.function(index)
            .and_then(|f| f.import.as_ref())
            .map(|(module, name)| format!("{module}.{name}"))
    }

    pub fn display_function_name(&self, index: u32) -> String {
        self.function_name(index)
            .unwrap_or_else(|| format!("func[{index}]"))
    }

    /// Resolves a user supplied function reference (name, export, `func[N]` or `$N`).
    pub fn find_functions(&self, query: &str) -> Vec<u32> {
        let query = query.trim();
        if let Some(index) = parse_index_reference(query, "func") {
            return self
                .function(index)
                .map(|f| vec![f.index])
                .unwrap_or_default();
        }
        let mut found: Vec<u32> = self
            .functions
            .iter()
            .filter(|f| {
                let index = f.index;
                self.raw_function_name(index).is_some_and(|raw| {
                    raw == query || demangle(raw) == query || demangled_suffix_matches(raw, query)
                }) || self
                    .exports
                    .iter()
                    .any(|e| e.kind == ExternKind::Func && e.index == index && e.name == query)
            })
            .map(|f| f.index)
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    }

    pub fn exported_function(&self, name: &str) -> Option<u32> {
        self.exports
            .iter()
            .find(|e| e.kind == ExternKind::Func && e.name == name)
            .map(|e| e.index)
    }

    pub fn local_name(&self, function: u32, local: u32) -> Option<&str> {
        self.names
            .locals
            .get(&function)
            .and_then(|m| m.get(&local))
            .map(String::as_str)
    }

    /// Type of a local (parameters first, then declared locals).
    pub fn local_type(&self, function: u32, local: u32) -> Option<ValueType> {
        let sig = self.func_sig(function)?;
        if let Some(ty) = sig.params.get(local as usize) {
            return Some(ty.clone());
        }
        let body = self.function(function)?.body.as_ref()?;
        body.declared_local_type(local - sig.params.len() as u32)
            .cloned()
    }

    pub fn global_name(&self, index: u32) -> Option<String> {
        if let Some(name) = self.names.globals.get(&index) {
            return Some(name.clone());
        }
        if let Some(export) = self
            .exports
            .iter()
            .find(|e| e.kind == ExternKind::Global && e.index == index)
        {
            return Some(export.name.clone());
        }
        self.imports
            .iter()
            .find(|i| i.kind == ExternKind::Global && i.index == index)
            .map(|i| format!("{}.{}", i.module, i.name))
    }

    pub fn find_global(&self, name: &str) -> Option<u32> {
        if let Some(index) = parse_index_reference(name, "global") {
            return ((index as usize) < self.globals.len()).then_some(index);
        }
        (0..self.globals.len() as u32).find(|i| self.global_name(*i).as_deref() == Some(name))
    }

    pub fn memory_name(&self, index: u32) -> String {
        self.names
            .memories
            .get(&index)
            .cloned()
            .or_else(|| {
                self.exports
                    .iter()
                    .find(|e| e.kind == ExternKind::Memory && e.index == index)
                    .map(|e| e.name.clone())
            })
            .unwrap_or_else(|| format!("memory{index}"))
    }

    pub fn has_dwarf(&self) -> bool {
        self.custom_sections.iter().any(|s| s.name == ".debug_info")
            && self.custom_sections.iter().any(|s| s.name == ".debug_line")
    }

    /// Lazily computed text-format disassembly with offset mappings.
    pub fn disassembly(&self) -> Result<Arc<Disassembly>> {
        self.disassembly
            .get_or_init(|| {
                Disassembly::new(self)
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(Error::InvalidModule)
    }
}

fn table_info(t: &wasmparser::TableType, imported: bool) -> TableInfo {
    TableInfo {
        element: t.element_type.to_string(),
        initial: t.initial,
        maximum: t.maximum,
        table64: t.table64,
        imported,
    }
}

fn memory_info(m: &wasmparser::MemoryType, imported: bool) -> MemoryInfo {
    MemoryInfo {
        initial: m.initial,
        maximum: m.maximum,
        memory64: m.memory64,
        shared: m.shared,
        page_size: 1u64 << m.page_size_log2.unwrap_or(16),
        imported,
    }
}

fn read_body(body: &wasmparser::FunctionBody<'_>) -> Result<FunctionBody> {
    let range = body.range();
    let mut locals = Vec::new();
    for local in body.get_locals_reader()? {
        let (count, ty) = local?;
        locals.push((count, ty.into()));
    }
    let mut instructions = Vec::new();
    let mut reader = body.get_operators_reader()?;
    while !reader.eof() {
        let (op, offset) = reader.read_with_offset()?;
        let kind = match op {
            Operator::Call { .. } | Operator::CallIndirect { .. } | Operator::CallRef { .. } => {
                InstrKind::Call
            }
            Operator::ReturnCall { .. }
            | Operator::ReturnCallIndirect { .. }
            | Operator::ReturnCallRef { .. } => InstrKind::TailCall,
            Operator::Return => InstrKind::Return,
            Operator::Loop { .. } => InstrKind::Loop,
            _ => InstrKind::Other,
        };
        instructions.push(Instruction {
            offset: offset as u32,
            kind,
        });
    }
    Ok(FunctionBody {
        range: range.start as u32..range.end as u32,
        locals,
        instructions,
    })
}

fn describe_const_expr(expr: &ConstExpr<'_>) -> Option<String> {
    let mut reader = expr.get_operators_reader();
    let op = reader.read().ok()?;
    Some(match op {
        Operator::I32Const { value } => format!("i32.const {value}"),
        Operator::I64Const { value } => format!("i64.const {value}"),
        Operator::F32Const { value } => format!("f32.const {}", f32::from_bits(value.bits())),
        Operator::F64Const { value } => format!("f64.const {}", f64::from_bits(value.bits())),
        Operator::GlobalGet { global_index } => format!("global.get {global_index}"),
        Operator::RefNull { .. } => "ref.null".to_string(),
        Operator::RefFunc { function_index } => format!("ref.func {function_index}"),
        other => format!("{other:?}"),
    })
}

fn const_expr_value(expr: &ConstExpr<'_>) -> Option<u64> {
    let mut reader = expr.get_operators_reader();
    match reader.read().ok()? {
        Operator::I32Const { value } => Some(value as u32 as u64),
        Operator::I64Const { value } => Some(value as u64),
        _ => None,
    }
}

fn parse_index_reference(text: &str, prefix: &str) -> Option<u32> {
    if let Some(rest) = text.strip_prefix(prefix) {
        let inner = rest.strip_prefix('[')?.strip_suffix(']')?;
        return inner.trim().parse().ok();
    }
    text.strip_prefix('$')?.parse().ok()
}

fn demangled_suffix_matches(raw: &str, query: &str) -> bool {
    let demangled = demangle(raw);
    demangled
        .rsplit_once("::")
        .is_some_and(|(_, last)| last == query)
}
