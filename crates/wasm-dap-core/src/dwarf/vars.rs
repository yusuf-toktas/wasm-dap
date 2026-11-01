//! Source variables visible at a program counter.

use gimli::Reader as _;
use gimli::{AttributeValue, DebuggingInformationEntry, UnitOffset};

use super::types::TypeId;
use super::{DebugInfo, Reader};

const MAX_SCOPE_DEPTH: usize = 256;

/// How to find a variable's value.
#[derive(Debug, Clone)]
pub enum VariableLocation {
    Expr(gimli::Expression<Reader>),
    /// Location list entries as (start, end, expression) with module offsets.
    List(Vec<(u32, u32, gimli::Expression<Reader>)>),
    /// Compile-time constant value (little-endian bytes).
    Const(Vec<u8>),
    Missing,
}

#[derive(Debug, Clone)]
pub struct Variable {
    pub name: String,
    pub ty: Option<TypeId>,
    pub location: VariableLocation,
    pub parameter: bool,
    pub decl_line: Option<u32>,
}

impl Variable {
    /// Location expression that applies at `pc`, if any.
    pub fn expression_at(&self, pc: u32) -> Option<&gimli::Expression<Reader>> {
        match &self.location {
            VariableLocation::Expr(e) => Some(e),
            VariableLocation::List(entries) => entries
                .iter()
                .find(|(start, end, _)| *start <= pc && pc < *end)
                .map(|(_, _, e)| e),
            _ => None,
        }
    }
}

/// Variables of the innermost function frame containing a program counter.
#[derive(Debug, Clone)]
pub struct FrameVariables {
    pub function: String,
    pub unit: usize,
    pub encoding: gimli::Encoding,
    pub frame_base: Option<gimli::Expression<Reader>>,
    pub variables: Vec<Variable>,
}

impl DebugInfo {
    /// Parameters and locals in scope at `pc`, outermost scope first.
    pub fn frame_variables(&self, pc: u32) -> Option<FrameVariables> {
        let subprogram = self.subprogram_at(pc)?;
        let unit_index = subprogram.unit;
        let unit = &self.units[unit_index].unit;
        let mut tree = unit
            .entries_tree(Some(UnitOffset(subprogram.offset)))
            .ok()?;
        let root = tree.root().ok()?;
        let entry = root.entry().clone();
        let frame_base = match entry.attr_value(gimli::DW_AT_frame_base) {
            Some(AttributeValue::Exprloc(expr)) => Some(expr),
            _ => None,
        };

        let mut variables = Vec::new();
        self.collect_scope(unit_index, root, pc, 0, &mut variables);

        Some(FrameVariables {
            function: subprogram.name.clone(),
            unit: unit_index,
            encoding: unit.encoding(),
            frame_base,
            variables,
        })
    }

    fn collect_scope(
        &self,
        unit_index: usize,
        node: gimli::EntriesTreeNode<'_, '_, Reader>,
        pc: u32,
        depth: usize,
        out: &mut Vec<Variable>,
    ) {
        let mut nested = Vec::new();
        let mut children = node.children();
        while let Ok(Some(child)) = children.next() {
            let entry = child.entry().clone();
            match entry.tag() {
                gimli::DW_TAG_formal_parameter | gimli::DW_TAG_variable => {
                    if let Some(variable) = self.parse_variable(unit_index, &entry) {
                        out.push(variable);
                    }
                }
                // The depth limit keeps malformed DWARF from exhausting the stack.
                gimli::DW_TAG_lexical_block
                    if depth < MAX_SCOPE_DEPTH && self.die_contains(unit_index, &entry, pc) =>
                {
                    let mut inner = Vec::new();
                    self.collect_scope(unit_index, child, pc, depth + 1, &mut inner);
                    nested.push(inner);
                }
                _ => {}
            }
        }
        for inner in nested {
            out.extend(inner);
        }
    }

    fn die_contains(
        &self,
        unit_index: usize,
        entry: &DebuggingInformationEntry<Reader>,
        pc: u32,
    ) -> bool {
        let unit = &self.units[unit_index].unit;
        let Ok(mut ranges) = self.dwarf.die_ranges(unit, entry) else {
            return false;
        };
        while let Ok(Some(range)) = ranges.next() {
            if let (Some(start), Some(end)) = (self.to_pc(range.begin), self.to_pc(range.end))
                && start <= pc
                && pc < end
            {
                return true;
            }
        }
        false
    }

    fn parse_variable(
        &self,
        unit_index: usize,
        entry: &DebuggingInformationEntry<Reader>,
    ) -> Option<Variable> {
        let origin = self.abstract_origin(unit_index, entry);
        let lookup = |attr| {
            entry
                .attr_value(attr)
                .or_else(|| origin.as_ref().and_then(|(_, o)| o.attr_value(attr)))
        };
        let name = self
            .die_name(unit_index, entry)
            .or_else(|| origin.as_ref().and_then(|(u, o)| self.die_name(*u, o)))?;
        let ty = self
            .die_type(unit_index, entry)
            .or_else(|| origin.as_ref().and_then(|(u, o)| self.die_type(*u, o)));
        let unit = &self.units[unit_index].unit;

        let location = match entry.attr_value(gimli::DW_AT_location) {
            Some(AttributeValue::Exprloc(expr)) => VariableLocation::Expr(expr),
            Some(value) => match self.dwarf.attr_locations(unit, value) {
                Ok(Some(mut list)) => {
                    let mut entries = Vec::new();
                    while let Ok(Some(item)) = list.next() {
                        if let (Some(start), Some(end)) =
                            (self.to_pc(item.range.begin), self.to_pc(item.range.end))
                        {
                            entries.push((start, end, item.data));
                        }
                    }
                    VariableLocation::List(entries)
                }
                _ => VariableLocation::Missing,
            },
            None => match lookup(gimli::DW_AT_const_value) {
                Some(AttributeValue::Block(block)) => block
                    .to_slice()
                    .map(|b| VariableLocation::Const(b.into_owned()))
                    .unwrap_or(VariableLocation::Missing),
                Some(AttributeValue::Sdata(v)) => VariableLocation::Const(v.to_le_bytes().to_vec()),
                Some(AttributeValue::Udata(v)) => VariableLocation::Const(v.to_le_bytes().to_vec()),
                Some(v) => v
                    .udata_value()
                    .map(|u| VariableLocation::Const(u.to_le_bytes().to_vec()))
                    .unwrap_or(VariableLocation::Missing),
                None => VariableLocation::Missing,
            },
        };

        Some(Variable {
            name,
            ty,
            location,
            parameter: entry.tag() == gimli::DW_TAG_formal_parameter,
            decl_line: lookup(gimli::DW_AT_decl_line)
                .and_then(|v| v.udata_value())
                .map(|l| l as u32),
        })
    }

    pub(crate) fn abstract_origin(
        &self,
        unit_index: usize,
        entry: &DebuggingInformationEntry<Reader>,
    ) -> Option<(usize, DebuggingInformationEntry<Reader>)> {
        let value = entry
            .attr_value(gimli::DW_AT_abstract_origin)
            .or_else(|| entry.attr_value(gimli::DW_AT_specification))?;
        let (unit, offset) = self.reference(unit_index, value)?;
        let origin = self.units[unit].unit.entry(UnitOffset(offset)).ok()?;
        Some((unit, origin))
    }
}

impl DebugInfo {
    /// Static variables (with a fixed address) matching a simple or qualified name.
    pub fn global_variables(&self, name: &str) -> Vec<Variable> {
        let index = self.global_index.get_or_init(|| self.build_global_index());
        index
            .get(name)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(unit, offset)| {
                        let entry = self.units[*unit].unit.entry(UnitOffset(*offset)).ok()?;
                        self.parse_variable(*unit, &entry)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn build_global_index(&self) -> std::collections::HashMap<String, Vec<(usize, usize)>> {
        let mut index: std::collections::HashMap<String, Vec<(usize, usize)>> =
            std::collections::HashMap::new();
        for (unit_index, entry) in self.units.iter().enumerate() {
            let mut cursor = entry.unit.entries();
            let mut scopes: Vec<(isize, Option<String>)> = Vec::new();
            while let Ok(Some(die)) = cursor.next_dfs() {
                let die = die.clone();
                let depth = die.depth();
                while scopes.last().is_some_and(|(d, _)| *d >= depth) {
                    scopes.pop();
                }
                match die.tag() {
                    gimli::DW_TAG_namespace => {
                        scopes.push((depth, self.die_name(unit_index, &die)));
                    }
                    gimli::DW_TAG_subprogram | gimli::DW_TAG_lexical_block => {
                        scopes.push((depth, None));
                    }
                    gimli::DW_TAG_variable => {
                        let inside_function = scopes.iter().any(|(_, n)| n.is_none());
                        let has_address = matches!(
                            die.attr_value(gimli::DW_AT_location),
                            Some(AttributeValue::Exprloc(_))
                        );
                        if inside_function || !has_address {
                            continue;
                        }
                        let Some(name) = self.die_name(unit_index, &die) else {
                            continue;
                        };
                        let location = (unit_index, die.offset().0);
                        let mut qualified: Vec<&str> =
                            scopes.iter().filter_map(|(_, n)| n.as_deref()).collect();
                        qualified.push(&name);
                        let full = qualified.join("::");
                        if full != name {
                            index.entry(full).or_default().push(location);
                        }
                        index.entry(name).or_default().push(location);
                    }
                    _ => {}
                }
            }
        }
        index
    }
}
