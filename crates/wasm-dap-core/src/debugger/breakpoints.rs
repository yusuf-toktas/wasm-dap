//! Breakpoint bookkeeping on the adapter side.
//!
//! DAP breakpoints are resolved to module offsets here; the debuggee thread
//! only sees the resulting [`BreakpointSet`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::memory::{format_code_address, parse_address};
use super::program::ProgramInfo;
use crate::dwarf::paths;
use crate::module::InstrKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakpointKind {
    Source,
    Function,
    Instruction,
}

/// Controls how many hits of a breakpoint are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitCondition {
    AtLeast(u64),
    Equal(u64),
    Greater(u64),
    Less(u64),
    LessOrEqual(u64),
    Every(u64),
}

impl HitCondition {
    /// Parses `N` (same as `>= N`), `== N`, `> N`, `>= N`, `< N`, `<= N` or `% N`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (op, rest) = ["==", ">=", "<=", ">", "<", "%", "="]
            .iter()
            .find_map(|op| text.strip_prefix(op).map(|rest| (*op, rest)))
            .unwrap_or(("", text));
        let value: u64 = rest.trim().parse().map_err(|_| {
            format!("invalid hit condition `{text}`; use N, ==N, >N, >=N, <N, <=N or %N")
        })?;
        Ok(match op {
            "" | ">=" => HitCondition::AtLeast(value),
            "==" | "=" => HitCondition::Equal(value),
            ">" => HitCondition::Greater(value),
            "<" => HitCondition::Less(value),
            "<=" => HitCondition::LessOrEqual(value),
            "%" => {
                if value == 0 {
                    return Err("hit condition `% 0` is invalid".into());
                }
                HitCondition::Every(value)
            }
            _ => unreachable!(),
        })
    }

    pub fn matches(self, hits: u64) -> bool {
        match self {
            HitCondition::AtLeast(n) => hits >= n,
            HitCondition::Equal(n) => hits == n,
            HitCondition::Greater(n) => hits > n,
            HitCondition::Less(n) => hits < n,
            HitCondition::LessOrEqual(n) => hits <= n,
            HitCondition::Every(n) => hits.is_multiple_of(n),
        }
    }
}

/// A breakpoint attached to an address, as evaluated by the debuggee thread.
#[derive(Debug, Clone)]
pub struct BreakpointSpec {
    pub id: i64,
    pub kind: BreakpointKind,
    pub condition: Option<String>,
    pub hit_condition: Option<HitCondition>,
    pub log_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionFilters {
    /// Stop when WebAssembly code traps.
    pub traps: bool,
    /// Stop when a host function fails, e.g. a call to an unresolved import.
    pub host_errors: bool,
}

impl Default for ExceptionFilters {
    fn default() -> Self {
        Self {
            traps: true,
            host_errors: true,
        }
    }
}

pub const FILTER_TRAPS: &str = "traps";
pub const FILTER_HOST_ERRORS: &str = "hostErrors";

/// Every resolved breakpoint address together with its breakpoints.
#[derive(Debug, Clone, Default)]
pub struct BreakpointSet {
    pub by_address: BTreeMap<u32, Vec<BreakpointSpec>>,
    pub exceptions: ExceptionFilters,
}

impl BreakpointSet {
    pub fn addresses(&self) -> impl Iterator<Item = u32> + '_ {
        self.by_address.keys().copied()
    }
}

/// Where a source breakpoint was requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceTarget {
    /// A file on disk, matched against DWARF line tables.
    File(String),
    /// The module's virtual text-format document.
    Wat,
}

#[derive(Debug, Clone, Default)]
pub struct SourceBreakpointRequest {
    pub line: u32,
    pub column: Option<u32>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FunctionBreakpointRequest {
    pub name: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct InstructionBreakpointRequest {
    pub instruction_reference: String,
    pub offset: i64,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

/// Verification result reported back to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointStatus {
    pub id: i64,
    pub verified: bool,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: Option<String>,
    pub instruction_reference: Option<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    key: String,
    status: BreakpointStatus,
    spec: BreakpointSpec,
    addresses: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct BreakpointManager {
    next_id: i64,
    sources: HashMap<String, Vec<Entry>>,
    functions: Vec<Entry>,
    instructions: Vec<Entry>,
    exceptions: ExceptionFilters,
}

impl BreakpointManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn reuse_id(previous: &[Entry], key: &str, next_id: &mut i64) -> i64 {
        previous
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.spec.id)
            .unwrap_or_else(|| {
                *next_id += 1;
                *next_id
            })
    }

    /// Replaces all breakpoints of one source.
    pub fn set_source(
        &mut self,
        program: &ProgramInfo,
        target: &SourceTarget,
        requests: &[SourceBreakpointRequest],
    ) -> Vec<BreakpointStatus> {
        let source_key = match target {
            SourceTarget::File(path) => paths::key(path),
            SourceTarget::Wat => "<wat>".to_string(),
        };
        let previous = self.sources.remove(&source_key).unwrap_or_default();
        let mut entries = Vec::with_capacity(requests.len());
        for request in requests {
            let key = format!("{}:{}", request.line, request.column.unwrap_or(0));
            let id = Self::reuse_id(&previous, &key, &mut self.next_id);
            let error = validate_conditions(
                request.condition.as_deref(),
                request.hit_condition.as_deref(),
            );
            let resolution = match &error {
                Some(_) => Resolution::default(),
                None => resolve_source(program, target, request.line),
            };
            let hit_condition = request
                .hit_condition
                .as_deref()
                .and_then(|h| HitCondition::parse(h).ok());
            let message = error.or(resolution.message.clone());
            let verified = message.is_none() && !resolution.addresses.is_empty();
            entries.push(Entry {
                key,
                status: BreakpointStatus {
                    id,
                    verified,
                    line: Some(resolution.line.unwrap_or(request.line)),
                    column: resolution.column.or(request.column),
                    message,
                    instruction_reference: resolution
                        .addresses
                        .first()
                        .map(|a| format_code_address(*a)),
                },
                spec: BreakpointSpec {
                    id,
                    kind: BreakpointKind::Source,
                    condition: non_empty(request.condition.clone()),
                    hit_condition,
                    log_message: non_empty(request.log_message.clone()),
                },
                addresses: if verified {
                    resolution.addresses
                } else {
                    Vec::new()
                },
            });
        }
        let statuses = entries.iter().map(|e| e.status.clone()).collect();
        if !entries.is_empty() {
            self.sources.insert(source_key, entries);
        }
        statuses
    }

    /// Replaces all function breakpoints.
    pub fn set_functions(
        &mut self,
        program: &ProgramInfo,
        requests: &[FunctionBreakpointRequest],
    ) -> Vec<BreakpointStatus> {
        let previous = std::mem::take(&mut self.functions);
        for request in requests {
            let key = request.name.trim().to_string();
            let id = Self::reuse_id(&previous, &key, &mut self.next_id);
            let error = validate_conditions(
                request.condition.as_deref(),
                request.hit_condition.as_deref(),
            );
            let addresses = match &error {
                Some(_) => Vec::new(),
                None => resolve_function(program, &key),
            };
            let message = match (error, addresses.is_empty()) {
                (Some(e), _) => Some(e),
                (None, true) => Some(format!("function `{key}` not found in module")),
                (None, false) => None,
            };
            let source = addresses.first().and_then(|a| program.dwarf_line(*a));
            self.functions.push(Entry {
                key,
                status: BreakpointStatus {
                    id,
                    verified: message.is_none(),
                    line: source.as_ref().map(|(l, _)| l.line),
                    column: None,
                    message,
                    instruction_reference: addresses.first().map(|a| format_code_address(*a)),
                },
                spec: BreakpointSpec {
                    id,
                    kind: BreakpointKind::Function,
                    condition: non_empty(request.condition.clone()),
                    hit_condition: request
                        .hit_condition
                        .as_deref()
                        .and_then(|h| HitCondition::parse(h).ok()),
                    log_message: None,
                },
                addresses,
            });
        }
        self.functions.iter().map(|e| e.status.clone()).collect()
    }

    /// Replaces all instruction breakpoints.
    pub fn set_instructions(
        &mut self,
        program: &ProgramInfo,
        requests: &[InstructionBreakpointRequest],
    ) -> Vec<BreakpointStatus> {
        let previous = std::mem::take(&mut self.instructions);
        for request in requests {
            let address = parse_address(&request.instruction_reference)
                .and_then(|base| u32::try_from(base as i64 + request.offset).ok());
            let key = address.map(|a| a.to_string()).unwrap_or_default();
            let id = Self::reuse_id(&previous, &key, &mut self.next_id);
            let error = validate_conditions(
                request.condition.as_deref(),
                request.hit_condition.as_deref(),
            );
            let valid = address.filter(|a| program.module.function_at(*a).is_some());
            let message = match (error, valid) {
                (Some(e), _) => Some(e),
                (None, None) => Some(format!(
                    "{} is not inside a function body",
                    request.instruction_reference
                )),
                _ => None,
            };
            let addresses: Vec<u32> = if message.is_none() {
                valid.into_iter().collect()
            } else {
                Vec::new()
            };
            let source = addresses.first().and_then(|a| program.dwarf_line(*a));
            self.instructions.push(Entry {
                key,
                status: BreakpointStatus {
                    id,
                    verified: message.is_none(),
                    line: source.as_ref().map(|(l, _)| l.line),
                    column: None,
                    message,
                    instruction_reference: addresses.first().map(|a| format_code_address(*a)),
                },
                spec: BreakpointSpec {
                    id,
                    kind: BreakpointKind::Instruction,
                    condition: non_empty(request.condition.clone()),
                    hit_condition: request
                        .hit_condition
                        .as_deref()
                        .and_then(|h| HitCondition::parse(h).ok()),
                    log_message: None,
                },
                addresses,
            });
        }
        self.instructions.iter().map(|e| e.status.clone()).collect()
    }

    pub fn set_exception_filters(&mut self, filters: &[String]) {
        self.exceptions = ExceptionFilters {
            traps: filters.iter().any(|f| f == FILTER_TRAPS),
            host_errors: filters.iter().any(|f| f == FILTER_HOST_ERRORS),
        };
    }

    pub fn exception_filters(&self) -> ExceptionFilters {
        self.exceptions
    }

    /// Snapshot handed to the debuggee thread.
    pub fn snapshot(&self) -> Arc<BreakpointSet> {
        let mut set = BreakpointSet {
            by_address: BTreeMap::new(),
            exceptions: self.exceptions,
        };
        let all = self
            .sources
            .values()
            .flatten()
            .chain(self.functions.iter())
            .chain(self.instructions.iter());
        for entry in all {
            for address in &entry.addresses {
                set.by_address
                    .entry(*address)
                    .or_default()
                    .push(entry.spec.clone());
            }
        }
        Arc::new(set)
    }
}

#[derive(Debug, Clone, Default)]
struct Resolution {
    line: Option<u32>,
    column: Option<u32>,
    addresses: Vec<u32>,
    message: Option<String>,
}

impl Resolution {
    fn failed(message: String) -> Self {
        Self {
            message: Some(message),
            ..Self::default()
        }
    }
}

fn resolve_source(program: &ProgramInfo, target: &SourceTarget, line: u32) -> Resolution {
    match target {
        SourceTarget::File(path) => {
            let Some(debug) = &program.debug else {
                return Resolution::failed(
                    "the module has no DWARF debug information; rebuild it with debug info".into(),
                );
            };
            let query = program
                .source_map
                .to_dwarf(path)
                .unwrap_or_else(|| paths::normalize(path));
            let files = debug.lines().find_files(&query);
            if files.is_empty() {
                return Resolution::failed(format!(
                    "{} is not part of the module's debug information (check `sourceMap`)",
                    paths::file_name(path)
                ));
            }
            match debug.lines().resolve_line(&files, line) {
                Some(resolved) => Resolution {
                    line: Some(resolved.line),
                    column: (resolved.column > 0).then_some(resolved.column),
                    addresses: resolved
                        .addresses
                        .into_iter()
                        .map(|a| skip_loop_headers(program, a))
                        .collect(),
                    message: None,
                },
                None => Resolution::failed(format!("no code was generated for line {line}")),
            }
        }
        SourceTarget::Wat => {
            let Ok(disassembly) = program.disassembly() else {
                return Resolution::failed("the module could not be disassembled".into());
            };
            match disassembly.next_instruction_line(line.saturating_sub(1), 64) {
                Some((actual, offset)) => Resolution {
                    line: Some(actual + 1),
                    column: None,
                    addresses: vec![offset],
                    message: None,
                },
                None => Resolution::failed(format!("no instruction on or after line {line}")),
            }
        }
    }
}

/// Moves a breakpoint off `loop` headers: compilers attribute a loop's first
/// line to the `loop` opcode, but back-edges branch past it, so a breakpoint
/// there would only trigger when the loop is entered, not on every iteration.
fn skip_loop_headers(program: &ProgramInfo, mut address: u32) -> u32 {
    while program.module.instruction_kind(address) == Some(InstrKind::Loop) {
        match program.module.next_instruction(address) {
            Some(next) => address = next,
            None => break,
        }
    }
    address
}

/// Breakpoint addresses for a function name: after the prologue when DWARF
/// marks one, so parameters are already stored when the breakpoint hits.
fn resolve_function(program: &ProgramInfo, name: &str) -> Vec<u32> {
    let mut functions: Vec<u32> = program.module.find_functions(name);
    if let Some(debug) = &program.debug {
        for subprogram in debug.find_subprograms(name) {
            if let Some(function) = program.module.function_at(subprogram.low) {
                functions.push(function.index);
            }
        }
    }
    functions.sort_unstable();
    functions.dedup();
    functions
        .into_iter()
        .filter_map(|index| function_entry_address(program, index))
        .collect()
}

pub(crate) fn function_entry_address(program: &ProgramInfo, index: u32) -> Option<u32> {
    let body = program.module.function(index)?.body.as_ref()?;
    let first = body.first_instruction()?;
    let after_prologue = program
        .debug
        .as_ref()
        .and_then(|d| d.lines().prologue_end_after(first))
        .filter(|a| *a < body.range.end && program.module.is_instruction(*a));
    Some(after_prologue.unwrap_or(first))
}

/// Returns an error message when a condition or hit condition cannot be parsed.
fn validate_conditions(condition: Option<&str>, hit: Option<&str>) -> Option<String> {
    if let Some(condition) = condition.filter(|c| !c.trim().is_empty())
        && let Err(e) = crate::evaluator::parse(condition)
    {
        return Some(format!("invalid condition: {e}"));
    }
    hit.filter(|h| !h.trim().is_empty())
        .and_then(|h| HitCondition::parse(h).err())
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|t| !t.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_conditions_parse_and_match() {
        assert_eq!(HitCondition::parse("3").unwrap(), HitCondition::AtLeast(3));
        assert!(HitCondition::parse("==2").unwrap().matches(2));
        assert!(!HitCondition::parse("== 2").unwrap().matches(3));
        assert!(HitCondition::parse("%2").unwrap().matches(4));
        assert!(!HitCondition::parse("%2").unwrap().matches(3));
        assert!(HitCondition::parse("< 2").unwrap().matches(1));
        assert!(HitCondition::parse("abc").is_err());
        assert!(HitCondition::parse("%0").is_err());
    }
}
