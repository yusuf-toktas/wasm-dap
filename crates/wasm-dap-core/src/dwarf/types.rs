//! DWARF type model, parsed lazily and cached per type DIE.

use std::sync::Arc;

use gimli::{AttributeValue, DebuggingInformationEntry, UnitOffset};

use super::{DebugInfo, Reader};

/// Reference to a type DIE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId {
    pub unit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseEncoding {
    Signed,
    Unsigned,
    Float,
    Boolean,
    SignedChar,
    UnsignedChar,
    Utf,
    Address,
    Other,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub name: Option<String>,
    pub ty: Option<TypeId>,
    pub offset: u64,
    /// Bit offset and bit size for bit-field members.
    pub bit_field: Option<(u64, u64)>,
    pub artificial: bool,
}

#[derive(Debug, Clone)]
pub struct Variant {
    pub discr_value: Option<u64>,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone)]
pub struct VariantPart {
    pub discriminant: Option<Member>,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone)]
pub enum TypeKind {
    Base(BaseEncoding),
    Pointer {
        pointee: Option<TypeId>,
        reference: bool,
    },
    Struct {
        members: Vec<Member>,
        variant_part: Option<VariantPart>,
        is_union: bool,
        template_params: Vec<(String, TypeId)>,
    },
    Array {
        element: Option<TypeId>,
        counts: Vec<Option<u64>>,
    },
    Enum {
        underlying: Option<TypeId>,
        enumerators: Vec<(String, i128)>,
    },
    Typedef(Option<TypeId>),
    Qualified(Option<TypeId>),
    Function,
    Unspecified,
}

#[derive(Debug, Clone)]
pub struct TypeDef {
    pub name: Option<String>,
    pub size: Option<u64>,
    pub kind: TypeKind,
    pub rust: bool,
}

impl DebugInfo {
    /// Parsed type definition for a type DIE.
    pub fn type_def(&self, id: TypeId) -> Option<Arc<TypeDef>> {
        if let Some(found) = self.type_cache.lock().ok()?.get(&id) {
            return Some(found.clone());
        }
        let parsed = Arc::new(self.parse_type(id)?);
        if let Ok(mut cache) = self.type_cache.lock() {
            cache.insert(id, parsed.clone());
        }
        Some(parsed)
    }

    /// Strips typedefs and cv-qualifiers.
    pub fn resolve_type(&self, mut id: TypeId) -> Option<(TypeId, Arc<TypeDef>)> {
        for _ in 0..32 {
            let def = self.type_def(id)?;
            match def.kind {
                TypeKind::Typedef(Some(target)) | TypeKind::Qualified(Some(target)) => id = target,
                _ => return Some((id, def)),
            }
        }
        None
    }

    /// Size of a type in bytes, following typedefs.
    pub fn type_size(&self, id: TypeId) -> Option<u64> {
        self.type_size_depth(id, 0)
    }

    // Malformed DWARF can describe self-referential types, hence the depth limit.
    fn type_size_depth(&self, id: TypeId, depth: u32) -> Option<u64> {
        if depth > 32 {
            return None;
        }
        let (_, def) = self.resolve_type(id)?;
        if let Some(size) = def.size {
            return Some(size);
        }
        match &def.kind {
            TypeKind::Pointer { .. } => Some(u64::from(self.address_size)),
            TypeKind::Array { element, counts } => {
                let element = self.type_size_depth((*element)?, depth + 1)?;
                counts
                    .iter()
                    .try_fold(element, |acc, c| c.map(|c| acc.saturating_mul(c)))
            }
            TypeKind::Enum {
                underlying: Some(u),
                ..
            } => self.type_size_depth(*u, depth + 1),
            _ => None,
        }
    }

    /// Display name of a type in the source language's style.
    pub fn type_name(&self, id: TypeId) -> String {
        self.type_name_depth(id, 0)
    }

    fn type_name_depth(&self, id: TypeId, depth: usize) -> String {
        if depth > 16 {
            return "...".to_string();
        }
        let Some(def) = self.type_def(id) else {
            return "?".to_string();
        };
        if let Some(name) = &def.name {
            return name.clone();
        }
        let inner = |t: &Option<TypeId>| {
            t.map(|t| self.type_name_depth(t, depth + 1))
                .unwrap_or_else(|| "void".to_string())
        };
        match &def.kind {
            TypeKind::Pointer { pointee, reference } => {
                let target = inner(pointee);
                match (def.rust, reference) {
                    (true, true) => format!("&{target}"),
                    (true, false) => format!("*mut {target}"),
                    (false, true) => format!("{target} &"),
                    (false, false) => format!("{target} *"),
                }
            }
            TypeKind::Array { element, counts } => {
                let element = inner(element);
                let dims: Vec<String> = counts
                    .iter()
                    .map(|c| c.map(|c| c.to_string()).unwrap_or_default())
                    .collect();
                if def.rust {
                    dims.iter()
                        .rev()
                        .fold(element, |acc, d| format!("[{acc}; {d}]"))
                } else {
                    let suffix: String = dims.iter().map(|d| format!("[{d}]")).collect();
                    format!("{element}{suffix}")
                }
            }
            TypeKind::Qualified(target) | TypeKind::Typedef(target) => inner(target),
            TypeKind::Function => "fn".to_string(),
            TypeKind::Struct { is_union: true, .. } => "union".to_string(),
            TypeKind::Struct { .. } => "struct".to_string(),
            TypeKind::Enum { .. } => "enum".to_string(),
            TypeKind::Base(_) | TypeKind::Unspecified => "void".to_string(),
        }
    }

    fn parse_type(&self, id: TypeId) -> Option<TypeDef> {
        let unit = &self.units.get(id.unit)?.unit;
        let entry = unit.entry(UnitOffset(id.offset)).ok()?;
        let rust = self.units[id.unit].rust;
        let name = self.die_name(id.unit, &entry);
        let size = entry
            .attr_value(gimli::DW_AT_byte_size)
            .and_then(|v| v.udata_value());
        let type_attr = self.die_type(id.unit, &entry);

        let kind = match entry.tag() {
            gimli::DW_TAG_base_type => {
                let encoding = match entry.attr_value(gimli::DW_AT_encoding) {
                    Some(AttributeValue::Encoding(e)) => match e {
                        gimli::DW_ATE_signed => BaseEncoding::Signed,
                        gimli::DW_ATE_unsigned => BaseEncoding::Unsigned,
                        gimli::DW_ATE_float => BaseEncoding::Float,
                        gimli::DW_ATE_boolean => BaseEncoding::Boolean,
                        gimli::DW_ATE_signed_char => BaseEncoding::SignedChar,
                        gimli::DW_ATE_unsigned_char => BaseEncoding::UnsignedChar,
                        gimli::DW_ATE_UTF => BaseEncoding::Utf,
                        gimli::DW_ATE_address => BaseEncoding::Address,
                        _ => BaseEncoding::Other,
                    },
                    _ => BaseEncoding::Other,
                };
                TypeKind::Base(encoding)
            }
            gimli::DW_TAG_pointer_type | gimli::DW_TAG_ptr_to_member_type => TypeKind::Pointer {
                pointee: type_attr,
                reference: false,
            },
            gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => {
                TypeKind::Pointer {
                    pointee: type_attr,
                    reference: true,
                }
            }
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type | gimli::DW_TAG_union_type => {
                let (members, variant_part, template_params) = self.parse_members(id)?;
                TypeKind::Struct {
                    members,
                    variant_part,
                    is_union: entry.tag() == gimli::DW_TAG_union_type,
                    template_params,
                }
            }
            gimli::DW_TAG_array_type => TypeKind::Array {
                element: type_attr,
                counts: self.parse_subranges(id)?,
            },
            gimli::DW_TAG_enumeration_type => TypeKind::Enum {
                underlying: type_attr,
                enumerators: self.parse_enumerators(id)?,
            },
            gimli::DW_TAG_typedef => TypeKind::Typedef(type_attr),
            gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_restrict_type
            | gimli::DW_TAG_atomic_type
            | gimli::DW_TAG_immutable_type => TypeKind::Qualified(type_attr),
            gimli::DW_TAG_subroutine_type => TypeKind::Function,
            _ => TypeKind::Unspecified,
        };
        Some(TypeDef {
            name,
            size,
            kind,
            rust,
        })
    }

    #[allow(clippy::type_complexity)]
    fn parse_members(
        &self,
        id: TypeId,
    ) -> Option<(Vec<Member>, Option<VariantPart>, Vec<(String, TypeId)>)> {
        let unit = &self.units[id.unit].unit;
        let mut tree = unit.entries_tree(Some(UnitOffset(id.offset))).ok()?;
        let root = tree.root().ok()?;
        let mut members = Vec::new();
        let mut variant_part = None;
        let mut template_params = Vec::new();
        let mut children = root.children();
        while let Ok(Some(child)) = children.next() {
            let entry = child.entry().clone();
            match entry.tag() {
                gimli::DW_TAG_member | gimli::DW_TAG_inheritance => {
                    if entry.attr_value(gimli::DW_AT_external).is_some()
                        || entry.attr_value(gimli::DW_AT_declaration).is_some()
                    {
                        continue;
                    }
                    members.push(self.parse_member(id.unit, &entry));
                }
                gimli::DW_TAG_template_type_parameter => {
                    if let (Some(name), Some(ty)) = (
                        self.die_name(id.unit, &entry),
                        self.die_type(id.unit, &entry),
                    ) {
                        template_params.push((name, ty));
                    }
                }
                gimli::DW_TAG_variant_part => {
                    let discr_offset = match entry.attr_value(gimli::DW_AT_discr) {
                        Some(AttributeValue::UnitRef(o)) => Some(o.0),
                        _ => None,
                    };
                    let mut part = VariantPart {
                        discriminant: None,
                        variants: Vec::new(),
                    };
                    let mut grand = child.children();
                    while let Ok(Some(node)) = grand.next() {
                        let inner = node.entry().clone();
                        match inner.tag() {
                            gimli::DW_TAG_member => {
                                if discr_offset == Some(inner.offset().0)
                                    || part.discriminant.is_none()
                                {
                                    part.discriminant = Some(self.parse_member(id.unit, &inner));
                                }
                            }
                            gimli::DW_TAG_variant => {
                                let discr_value =
                                    inner.attr_value(gimli::DW_AT_discr_value).and_then(|v| {
                                        v.udata_value().or(v.sdata_value().map(|s| s as u64))
                                    });
                                let mut variant_members = Vec::new();
                                let mut leaves = node.children();
                                while let Ok(Some(leaf)) = leaves.next() {
                                    let leaf_entry = leaf.entry();
                                    if leaf_entry.tag() == gimli::DW_TAG_member {
                                        variant_members
                                            .push(self.parse_member(id.unit, leaf_entry));
                                    }
                                }
                                part.variants.push(Variant {
                                    discr_value,
                                    members: variant_members,
                                });
                            }
                            _ => {}
                        }
                    }
                    variant_part = Some(part);
                }
                _ => {}
            }
        }
        Some((members, variant_part, template_params))
    }

    fn parse_member(&self, unit: usize, entry: &DebuggingInformationEntry<Reader>) -> Member {
        let offset = match entry.attr_value(gimli::DW_AT_data_member_location) {
            Some(AttributeValue::Exprloc(expr)) => {
                plus_uconst(&expr, self.units[unit].unit.encoding())
            }
            Some(value) => value
                .udata_value()
                .or(value.sdata_value().map(|s| s as u64)),
            None => None,
        }
        .unwrap_or(0);
        let bit_size = entry
            .attr_value(gimli::DW_AT_bit_size)
            .and_then(|v| v.udata_value());
        let bit_field = bit_size.map(|size| {
            let bit_offset = entry
                .attr_value(gimli::DW_AT_data_bit_offset)
                .and_then(|v| v.udata_value())
                .unwrap_or(offset * 8);
            (bit_offset, size)
        });
        let name = if entry.tag() == gimli::DW_TAG_inheritance {
            self.die_type(unit, entry)
                .map(|t| format!("<{}>", self.type_name(t)))
        } else {
            self.die_name(unit, entry)
        };
        Member {
            name,
            ty: self.die_type(unit, entry),
            offset: if bit_field.is_some() { 0 } else { offset },
            bit_field,
            artificial: entry.attr_value(gimli::DW_AT_artificial).is_some(),
        }
    }

    fn parse_subranges(&self, id: TypeId) -> Option<Vec<Option<u64>>> {
        let unit = &self.units[id.unit].unit;
        let mut tree = unit.entries_tree(Some(UnitOffset(id.offset))).ok()?;
        let root = tree.root().ok()?;
        let mut counts = Vec::new();
        let mut children = root.children();
        while let Ok(Some(child)) = children.next() {
            let entry = child.entry();
            if entry.tag() != gimli::DW_TAG_subrange_type {
                continue;
            }
            let lower = entry
                .attr_value(gimli::DW_AT_lower_bound)
                .and_then(|v| v.udata_value())
                .unwrap_or(0);
            let count = entry
                .attr_value(gimli::DW_AT_count)
                .and_then(|v| v.udata_value())
                .or_else(|| {
                    entry
                        .attr_value(gimli::DW_AT_upper_bound)
                        .and_then(|v| v.udata_value().or(v.sdata_value().map(|s| s as u64)))
                        .and_then(|upper| (upper.wrapping_add(1)).checked_sub(lower))
                });
            counts.push(count);
        }
        if counts.is_empty() {
            counts.push(None);
        }
        Some(counts)
    }

    fn parse_enumerators(&self, id: TypeId) -> Option<Vec<(String, i128)>> {
        let unit = &self.units[id.unit].unit;
        let mut tree = unit.entries_tree(Some(UnitOffset(id.offset))).ok()?;
        let root = tree.root().ok()?;
        let mut values = Vec::new();
        let mut children = root.children();
        while let Ok(Some(child)) = children.next() {
            let entry = child.entry();
            if entry.tag() != gimli::DW_TAG_enumerator {
                continue;
            }
            let Some(name) = self.die_name(id.unit, entry) else {
                continue;
            };
            let value = match entry.attr_value(gimli::DW_AT_const_value) {
                Some(AttributeValue::Sdata(v)) => i128::from(v),
                Some(AttributeValue::Udata(v)) => i128::from(v),
                Some(v) => v
                    .udata_value()
                    .map(i128::from)
                    .or(v.sdata_value().map(i128::from))
                    .unwrap_or(0),
                None => 0,
            };
            values.push((name, value));
        }
        Some(values)
    }
}

fn plus_uconst(expr: &gimli::Expression<Reader>, encoding: gimli::Encoding) -> Option<u64> {
    let mut ops = expr.clone().operations(encoding);
    let mut total = 0u64;
    while let Ok(Some(op)) = ops.next() {
        match op {
            gimli::Operation::PlusConstant { value } => total = total.wrapping_add(value),
            gimli::Operation::UnsignedConstant { value } => total = value,
            _ => return None,
        }
    }
    Some(total)
}
