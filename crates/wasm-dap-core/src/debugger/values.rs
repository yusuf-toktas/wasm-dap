//! Reading and formatting source-level values described by DWARF types.

use std::sync::Arc;

use wasmtime::{Instance, StoreContextMut};

use super::memory::{self, format_memory_reference};
use crate::dwarf::{BaseEncoding, DebugInfo, Member, TypeDef, TypeId, TypeKind, VariantPart};
use crate::error::{Error, Result};
use crate::runtime::HostState;

const STRING_LIMIT: usize = 1024;
const SUMMARY_ELEMENTS: usize = 8;
const SUMMARY_MEMBERS: usize = 6;
const MAX_CHILDREN: usize = 10_000;
const MAX_REGISTER_VALUE: usize = 4096;

/// Storage of a value: an address in linear memory or the bytes themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    Memory(u64),
    Bytes(Vec<u8>),
    Unavailable(String),
}

impl Place {
    fn offset(&self, offset: u64, size: Option<u64>) -> Place {
        match self {
            Place::Memory(address) => Place::Memory(address.wrapping_add(offset)),
            Place::Bytes(bytes) => {
                let start = (offset as usize).min(bytes.len());
                let end = match size {
                    Some(size) => start.saturating_add(size as usize).min(bytes.len()),
                    None => bytes.len(),
                };
                Place::Bytes(bytes[start..end].to_vec())
            }
            Place::Unavailable(reason) => Place::Unavailable(reason.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedValue {
    pub ty: Option<TypeId>,
    pub place: Place,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Children {
    None,
    Named(usize),
    Indexed(usize),
}

#[derive(Debug, Clone)]
pub(crate) struct Formatted {
    pub value: String,
    pub type_name: String,
    pub children: Children,
    pub memory_reference: Option<String>,
}

/// A child of a structured value.
#[derive(Debug, Clone)]
pub(crate) struct Child {
    pub name: String,
    pub value: TypedValue,
    /// Suffix appended to the parent's expression to name this child.
    pub accessor: String,
}

/// Scalar view of a value used by the expression evaluator.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Scalar {
    Int {
        value: i128,
        signed: bool,
        size: u8,
    },
    Float(f64),
    Bool(bool),
    Char(char),
    Pointer {
        address: u64,
        pointee: Option<TypeId>,
    },
}

/// Special layouts recognised on top of plain structs.
enum Shape {
    Str {
        ptr: u64,
        len: u64,
    },
    Slice {
        ptr: u64,
        len: u64,
        element: Option<TypeId>,
    },
    Vec {
        ptr: u64,
        len: u64,
        element: Option<TypeId>,
        string: bool,
    },
    Variant {
        name: String,
        base: u64,
        fields: Vec<Member>,
    },
    Plain,
}

pub(crate) struct ValueReader<'s, 'a> {
    pub store: &'s mut StoreContextMut<'a, HostState>,
    pub instance: Option<Instance>,
    pub debug: &'s Arc<DebugInfo>,
}

impl ValueReader<'_, '_> {
    fn pointer_size(&self) -> u64 {
        u64::from(self.debug.address_size())
    }

    pub fn read(&mut self, place: &Place, size: u64) -> Result<Vec<u8>> {
        let size = usize::try_from(size).map_err(|_| Error::Session("value too large".into()))?;
        match place {
            Place::Memory(address) => {
                let instance = self
                    .instance
                    .ok_or_else(|| Error::Session("no instance to read memory from".into()))?;
                memory::read_exact(self.store, instance, *address, size)
            }
            Place::Bytes(bytes) => {
                if size > bytes.len().max(MAX_REGISTER_VALUE) {
                    return Err(Error::Session("value too large".into()));
                }
                let mut out = bytes.clone();
                out.resize(size, 0);
                Ok(out)
            }
            Place::Unavailable(reason) => Err(Error::Session(reason.clone())),
        }
    }

    fn read_uint(&mut self, place: &Place, size: u64) -> Result<u64> {
        let bytes = self.read(place, size.min(8))?;
        let mut buf = [0u8; 8];
        buf[..bytes.len()].copy_from_slice(&bytes);
        Ok(u64::from_le_bytes(buf))
    }

    fn read_memory_lossy(&mut self, address: u64, len: usize) -> Vec<u8> {
        match self.instance {
            Some(instance) => {
                memory::read(self.store, instance, 0, address, len).unwrap_or_default()
            }
            None => Vec::new(),
        }
    }

    fn resolve(&self, ty: Option<TypeId>) -> Option<(TypeId, Arc<TypeDef>)> {
        self.debug.resolve_type(ty?)
    }

    /// Scalar interpretation of a value, if it has one.
    pub fn scalar(&mut self, value: &TypedValue) -> Result<Scalar> {
        let (_, def) = self
            .resolve(value.ty)
            .ok_or_else(|| Error::Eval("value has no known type".into()))?;
        match &def.kind {
            TypeKind::Base(encoding) => {
                let size = def.size.unwrap_or(4);
                let bytes = self.read(&value.place, size)?;
                Ok(base_scalar(*encoding, &bytes))
            }
            TypeKind::Pointer { pointee, .. } => Ok(Scalar::Pointer {
                address: self.read_uint(&value.place, self.pointer_size())?,
                pointee: *pointee,
            }),
            TypeKind::Enum { underlying, .. } => {
                let size = def
                    .size
                    .or_else(|| underlying.and_then(|u| self.debug.type_size(u)))
                    .unwrap_or(4);
                let signed = underlying
                    .and_then(|u| self.resolve(Some(u)))
                    .is_some_and(|(_, d)| matches!(d.kind, TypeKind::Base(BaseEncoding::Signed)));
                let bytes = self.read(&value.place, size)?;
                Ok(Scalar::Int {
                    value: int_from_bytes(&bytes, signed),
                    signed,
                    size: size as u8,
                })
            }
            _ => Err(Error::Eval(format!(
                "a value of type `{}` cannot be used as a number",
                value
                    .ty
                    .map(|t| self.debug.type_name(t))
                    .unwrap_or_default()
            ))),
        }
    }

    pub fn format(&mut self, value: &TypedValue, hex: bool) -> Formatted {
        self.format_depth(value, hex, 0)
    }

    fn format_depth(&mut self, value: &TypedValue, hex: bool, depth: u32) -> Formatted {
        let type_name = value
            .ty
            .map(|t| self.debug.type_name(t))
            .unwrap_or_else(|| "void".to_string());
        let mut out = Formatted {
            value: String::new(),
            type_name,
            children: Children::None,
            memory_reference: match value.place {
                Place::Memory(address) => Some(format_memory_reference(0, address)),
                _ => None,
            },
        };
        if let Place::Unavailable(reason) = &value.place {
            out.value = format!("<{reason}>");
            return out;
        }
        let Some((_, def)) = self.resolve(value.ty) else {
            out.value = "<unknown type>".to_string();
            return out;
        };
        let result = self.format_kind(value, &def, hex, depth, &mut out);
        if let Err(e) = result {
            out.value = format!("<error: {e}>");
            out.children = Children::None;
        }
        out
    }

    fn format_kind(
        &mut self,
        value: &TypedValue,
        def: &TypeDef,
        hex: bool,
        depth: u32,
        out: &mut Formatted,
    ) -> Result<()> {
        match &def.kind {
            TypeKind::Base(encoding) => {
                let size = def.size.unwrap_or(0);
                let bytes = self.read(&value.place, size)?;
                out.value = format_base(*encoding, &bytes, hex);
            }
            TypeKind::Pointer { pointee, .. } => {
                let address = self.read_uint(&value.place, self.pointer_size())?;
                out.value = format_address(address);
                out.memory_reference = Some(format_memory_reference(0, address));
                if address != 0 {
                    if let Some(text) = self.c_string_preview(*pointee, address) {
                        out.value = format!("{} {}", out.value, quote(&text));
                    }
                    let pointee_known =
                        pointee
                            .and_then(|p| self.resolve(Some(p)))
                            .is_some_and(|(_, d)| {
                                !matches!(d.kind, TypeKind::Unspecified | TypeKind::Function)
                            });
                    if pointee_known {
                        out.children = Children::Named(1);
                    }
                }
            }
            TypeKind::Struct { members, .. } => match self.shape(value, def)? {
                Shape::Str { ptr, len } => {
                    let bytes = self.read_memory_lossy(ptr, (len as usize).min(STRING_LIMIT));
                    out.value = quote(&String::from_utf8_lossy(&bytes));
                    out.children = Children::Named(2);
                }
                Shape::Vec {
                    ptr,
                    len,
                    string: true,
                    ..
                } => {
                    let bytes = self.read_memory_lossy(ptr, (len as usize).min(STRING_LIMIT));
                    out.value = quote(&String::from_utf8_lossy(&bytes));
                    out.children = Children::Indexed(len as usize);
                    out.memory_reference = Some(format_memory_reference(0, ptr));
                }
                Shape::Slice { ptr, len, element }
                | Shape::Vec {
                    ptr, len, element, ..
                } => {
                    out.value = self.summarize_elements(element, ptr, len, hex, depth);
                    out.children = Children::Indexed(len as usize);
                    out.memory_reference = Some(format_memory_reference(0, ptr));
                }
                Shape::Variant { name, base, fields } => {
                    out.value = if depth > 1 {
                        name.clone()
                    } else {
                        let place = value.place.offset(base, None);
                        self.summarize_members(&name, &fields, &place, hex, depth)
                    };
                    if !fields.is_empty() {
                        out.children = Children::Named(fields.len());
                    }
                }
                Shape::Plain => {
                    let visible: Vec<Member> = visible_members(members);
                    out.value = if depth > 1 {
                        "{…}".to_string()
                    } else {
                        self.summarize_members("", &visible, &value.place, hex, depth)
                    };
                    if !visible.is_empty() {
                        out.children = Children::Named(visible.len());
                    }
                }
            },
            TypeKind::Array { element, counts } => {
                let count = element_count(counts);
                let element_size = element.and_then(|e| self.debug.type_size(e)).unwrap_or(0);
                if let Some(count) = count
                    && self.is_c_char(*element)
                {
                    let bytes = self.read(&value.place, count.min(STRING_LIMIT as u64))?;
                    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                    out.value = quote(&String::from_utf8_lossy(&bytes[..end]));
                    out.children = Children::Indexed(count as usize);
                    return Ok(());
                }
                match (count, &value.place) {
                    (Some(count), Place::Memory(address)) => {
                        out.value = self.summarize_elements(*element, *address, count, hex, depth);
                        out.children = Children::Indexed(count as usize);
                    }
                    (Some(count), _) if depth > 1 => {
                        out.value = format!("[…; {count}]");
                        out.children = Children::Indexed(count as usize);
                    }
                    (Some(count), place) => {
                        let items: Vec<String> = (0..count.min(SUMMARY_ELEMENTS as u64))
                            .map(|i| {
                                let child = TypedValue {
                                    ty: *element,
                                    place: place
                                        .offset(i.wrapping_mul(element_size), Some(element_size)),
                                };
                                self.format_depth(&child, hex, depth + 1).value
                            })
                            .collect();
                        out.value = bracket(&items, count as usize);
                        out.children = Children::Indexed(count as usize);
                    }
                    (None, _) => out.value = "[…]".to_string(),
                }
            }
            TypeKind::Enum {
                underlying,
                enumerators,
            } => {
                let size = def
                    .size
                    .or_else(|| underlying.and_then(|u| self.debug.type_size(u)))
                    .unwrap_or(4);
                let bytes = self.read(&value.place, size)?;
                let raw = int_from_bytes(&bytes, true);
                let unsigned = int_from_bytes(&bytes, false);
                out.value = enumerators
                    .iter()
                    .find(|(_, v)| *v == raw || *v == unsigned)
                    .map(|(name, _)| name.clone())
                    .unwrap_or_else(|| raw.to_string());
            }
            TypeKind::Function => {
                let address = self.read_uint(&value.place, self.pointer_size())?;
                out.value = format!("fn #{address}");
            }
            TypeKind::Typedef(_) | TypeKind::Qualified(_) | TypeKind::Unspecified => {
                match def.size {
                    Some(size) if size > 0 && size <= 64 => {
                        let bytes = self.read(&value.place, size)?;
                        out.value = hex_bytes(&bytes);
                    }
                    _ => out.value = "void".to_string(),
                }
            }
        }
        Ok(())
    }

    /// True for C's one-byte character types, whose arrays and pointers are strings.
    fn is_c_char(&self, ty: Option<TypeId>) -> bool {
        self.resolve(ty).is_some_and(|(_, def)| {
            matches!(
                def.kind,
                TypeKind::Base(BaseEncoding::SignedChar | BaseEncoding::UnsignedChar)
            ) && def.size == Some(1)
                && !def.rust
        })
    }

    fn c_string_preview(&mut self, pointee: Option<TypeId>, address: u64) -> Option<String> {
        if !self.is_c_char(pointee) {
            return None;
        }
        let bytes = self.read_memory_lossy(address, 256);
        let end = bytes.iter().position(|b| *b == 0)?;
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }

    fn summarize_elements(
        &mut self,
        element: Option<TypeId>,
        address: u64,
        count: u64,
        hex: bool,
        depth: u32,
    ) -> String {
        if depth > 1 {
            return format!("[…; {count}]");
        }
        let size = element.and_then(|e| self.debug.type_size(e)).unwrap_or(0);
        let items: Vec<String> = (0..count.min(SUMMARY_ELEMENTS as u64))
            .map(|i| {
                let child = TypedValue {
                    ty: element,
                    place: Place::Memory(address.wrapping_add(i.wrapping_mul(size))),
                };
                self.format_depth(&child, hex, depth + 1).value
            })
            .collect();
        bracket(&items, count as usize)
    }

    fn summarize_members(
        &mut self,
        name: &str,
        members: &[Member],
        place: &Place,
        hex: bool,
        depth: u32,
    ) -> String {
        if members.is_empty() {
            return if name.is_empty() {
                "{}".to_string()
            } else {
                name.to_string()
            };
        }
        let tuple = members
            .iter()
            .all(|m| m.name.as_deref().is_some_and(|n| n.starts_with("__")));
        let parts: Vec<String> = members
            .iter()
            .take(SUMMARY_MEMBERS)
            .map(|m| {
                let child = self.member_value(m, place);
                let text = self.format_depth(&child, hex, depth + 1).value;
                if tuple {
                    text
                } else {
                    format!("{}: {text}", m.name.as_deref().unwrap_or("?"))
                }
            })
            .collect();
        let more = if members.len() > SUMMARY_MEMBERS {
            ", …"
        } else {
            ""
        };
        match (tuple, name.is_empty()) {
            (true, _) => format!("{name}({}{more})", parts.join(", ")),
            (false, true) => format!("{{{}{more}}}", parts.join(", ")),
            (false, false) => format!("{name} {{ {}{more} }}", parts.join(", ")),
        }
    }

    fn member_value(&mut self, member: &Member, place: &Place) -> TypedValue {
        let size = member.ty.and_then(|t| self.debug.type_size(t));
        if let Some((bit_offset, bit_size)) = member.bit_field {
            let byte_start = bit_offset / 8;
            let span = (bit_offset % 8).saturating_add(bit_size).div_ceil(8);
            let value = self
                .read(&place.offset(byte_start, Some(span)), span)
                .map(|bytes| {
                    let raw = int_from_bytes(&bytes, false) as u128;
                    let shifted = raw >> (bit_offset % 8);
                    let mask = if bit_size >= 128 {
                        u128::MAX
                    } else {
                        (1u128 << bit_size) - 1
                    };
                    let field = shifted & mask;
                    let width = size.unwrap_or(span).max(1) as usize;
                    Place::Bytes(field.to_le_bytes()[..width.min(16)].to_vec())
                })
                .unwrap_or_else(|e| Place::Unavailable(e.to_string()));
            return TypedValue {
                ty: member.ty,
                place: value,
            };
        }
        TypedValue {
            ty: member.ty,
            place: place.offset(member.offset, size),
        }
    }

    fn shape(&mut self, value: &TypedValue, def: &TypeDef) -> Result<Shape> {
        let TypeKind::Struct {
            members,
            variant_part,
            template_params,
            ..
        } = &def.kind
        else {
            return Ok(Shape::Plain);
        };
        let name = def.name.as_deref().unwrap_or("");
        if let Some(part) = variant_part {
            return self.active_variant(value, part);
        }
        if !def.rust {
            return Ok(Shape::Plain);
        }
        let find = |n: &str| members.iter().find(|m| m.name.as_deref() == Some(n));
        if let (Some(ptr), Some(len)) = (find("data_ptr"), find("length")) {
            let address =
                self.read_uint(&value.place.offset(ptr.offset, None), self.pointer_size())?;
            let length =
                self.read_uint(&value.place.offset(len.offset, None), self.pointer_size())?;
            if name == "&str" || name == "&mut str" {
                return Ok(Shape::Str {
                    ptr: address,
                    len: length,
                });
            }
            let element = ptr
                .ty
                .and_then(|t| self.resolve(Some(t)))
                .and_then(|(_, d)| match d.kind {
                    TypeKind::Pointer { pointee, .. } => pointee,
                    _ => None,
                });
            return Ok(Shape::Slice {
                ptr: address,
                len: length,
                element,
            });
        }
        let is_string = name == "String" || name.ends_with("::String");
        let is_vec = name.starts_with("Vec<") || name.contains("::Vec<");
        if is_string || is_vec {
            let vec_value;
            let (vec_members, vec_def_params, base) = if is_string {
                let Some(inner) = find("vec") else {
                    return Ok(Shape::Plain);
                };
                let Some((_, inner_def)) = self.resolve(inner.ty) else {
                    return Ok(Shape::Plain);
                };
                vec_value = inner_def;
                match &vec_value.kind {
                    TypeKind::Struct {
                        members,
                        template_params,
                        ..
                    } => (members.clone(), template_params.clone(), inner.offset),
                    _ => return Ok(Shape::Plain),
                }
            } else {
                (members.clone(), template_params.clone(), 0)
            };
            let len_member = vec_members
                .iter()
                .find(|m| m.name.as_deref() == Some("len"));
            let buf = vec_members
                .iter()
                .find(|m| m.name.as_deref() == Some("buf"));
            let element = vec_def_params
                .iter()
                .find(|(n, _)| n == "T")
                .map(|(_, t)| *t);
            if let (Some(len_member), Some(buf)) = (len_member, buf)
                && let Some(ptr_offset) = self.find_pointer_offset(buf.ty, 0)
            {
                let place = value.place.offset(base, None);
                let len =
                    self.read_uint(&place.offset(len_member.offset, None), self.pointer_size())?;
                let ptr = self.read_uint(
                    &place.offset(buf.offset.wrapping_add(ptr_offset), None),
                    self.pointer_size(),
                )?;
                return Ok(Shape::Vec {
                    ptr,
                    len,
                    element,
                    string: is_string,
                });
            }
        }
        Ok(Shape::Plain)
    }

    /// Offset of the first pointer found by descending into struct members.
    fn find_pointer_offset(&self, ty: Option<TypeId>, depth: u32) -> Option<u64> {
        if depth > 8 {
            return None;
        }
        let (_, def) = self.resolve(ty)?;
        match &def.kind {
            TypeKind::Pointer { .. } => Some(0),
            TypeKind::Struct { members, .. } => members.iter().find_map(|m| {
                self.find_pointer_offset(m.ty, depth + 1)
                    .map(|inner| m.offset.wrapping_add(inner))
            }),
            _ => None,
        }
    }

    fn active_variant(&mut self, value: &TypedValue, part: &VariantPart) -> Result<Shape> {
        let discriminant = match &part.discriminant {
            Some(member) => {
                let size = member.ty.and_then(|t| self.debug.type_size(t)).unwrap_or(4);
                Some(self.read_uint(&value.place.offset(member.offset, Some(size)), size)?)
            }
            None => None,
        };
        let variant = part
            .variants
            .iter()
            .find(|v| v.discr_value.is_some() && v.discr_value == discriminant)
            .or_else(|| part.variants.iter().find(|v| v.discr_value.is_none()));
        let Some(variant) = variant else {
            return Ok(Shape::Plain);
        };
        let Some(member) = variant.members.first() else {
            return Ok(Shape::Plain);
        };
        let name = member.name.clone().unwrap_or_default();
        let fields = member
            .ty
            .and_then(|t| self.resolve(Some(t)))
            .map(|(_, d)| match &d.kind {
                TypeKind::Struct { members, .. } => visible_members(members),
                _ => Vec::new(),
            })
            .unwrap_or_default();
        Ok(Shape::Variant {
            name,
            base: member.offset,
            fields,
        })
    }

    /// Children of a structured value, optionally paged for indexed children.
    pub fn children(
        &mut self,
        value: &TypedValue,
        start: usize,
        count: Option<usize>,
    ) -> Result<Vec<Child>> {
        let Some((_, def)) = self.resolve(value.ty) else {
            return Ok(Vec::new());
        };
        // Lengths read from uninitialized memory can be huge, so a single request
        // never materializes more than `MAX_CHILDREN` elements.
        let page = |len: usize| {
            let start = start.min(len);
            let wanted = count.unwrap_or(len - start).min(MAX_CHILDREN);
            start..start.saturating_add(wanted).min(len)
        };
        let indexed =
            |element: Option<TypeId>, base: Place, size: u64, range: std::ops::Range<usize>| {
                range
                    .map(|i| Child {
                        name: format!("[{i}]"),
                        value: TypedValue {
                            ty: element,
                            place: base.offset((i as u64).wrapping_mul(size), Some(size)),
                        },
                        accessor: format!("[{i}]"),
                    })
                    .collect::<Vec<_>>()
            };
        match &def.kind {
            TypeKind::Pointer { pointee, .. } => {
                let address = self.read_uint(&value.place, self.pointer_size())?;
                Ok(vec![Child {
                    name: "*".to_string(),
                    value: TypedValue {
                        ty: *pointee,
                        place: Place::Memory(address),
                    },
                    accessor: String::new(),
                }])
            }
            TypeKind::Array { element, counts } => {
                let total = element_count(counts).unwrap_or(0);
                let size = element.and_then(|e| self.debug.type_size(e)).unwrap_or(0);
                Ok(indexed(
                    *element,
                    value.place.clone(),
                    size,
                    page(total as usize),
                ))
            }
            TypeKind::Struct { members, .. } => match self.shape(value, &def)? {
                Shape::Slice { ptr, len, element }
                | Shape::Vec {
                    ptr, len, element, ..
                } => {
                    let size = element.and_then(|e| self.debug.type_size(e)).unwrap_or(1);
                    Ok(indexed(
                        element,
                        Place::Memory(ptr),
                        size,
                        page(len as usize),
                    ))
                }
                Shape::Variant { base, fields, .. } => {
                    let place = value.place.offset(base, None);
                    Ok(fields.iter().map(|m| self.named_child(m, &place)).collect())
                }
                Shape::Str { .. } | Shape::Plain => Ok(visible_members(members)
                    .iter()
                    .map(|m| self.named_child(m, &value.place))
                    .collect()),
            },
            _ => Ok(Vec::new()),
        }
    }

    fn named_child(&mut self, member: &Member, place: &Place) -> Child {
        let name = member
            .name
            .clone()
            .unwrap_or_else(|| "<anonymous>".to_string());
        let accessor = if name.starts_with('<') {
            String::new()
        } else if let Some(index) = name.strip_prefix("__") {
            format!(".{index}")
        } else {
            format!(".{name}")
        };
        Child {
            value: self.member_value(member, place),
            name,
            accessor,
        }
    }

    /// Number of elements of a string, slice, vector or array, following references.
    pub fn length(&mut self, value: &TypedValue) -> Result<u64> {
        let mut value = value.clone();
        // Bounded so that self-referential pointer types in malformed DWARF terminate.
        for _ in 0..8 {
            let (_, def) = self
                .resolve(value.ty)
                .ok_or_else(|| Error::Eval("value has no known type".into()))?;
            let TypeKind::Pointer { pointee, .. } = &def.kind else {
                return self.container_length(&value, &def);
            };
            let address = self.read_uint(&value.place, self.pointer_size())?;
            value = TypedValue {
                ty: *pointee,
                place: Place::Memory(address),
            };
        }
        Err(not_a_container())
    }

    fn container_length(&mut self, value: &TypedValue, def: &TypeDef) -> Result<u64> {
        match &def.kind {
            TypeKind::Array { counts, .. } => element_count(counts).ok_or_else(not_a_container),
            TypeKind::Struct { .. } => match self.shape(value, def)? {
                Shape::Str { len, .. } | Shape::Slice { len, .. } | Shape::Vec { len, .. } => {
                    Ok(len)
                }
                _ => Err(not_a_container()),
            },
            _ => Err(not_a_container()),
        }
    }

    /// Finds a member (or active-variant field) by name.
    pub fn member(&mut self, value: &TypedValue, name: &str) -> Result<TypedValue> {
        let children = self.children(value, 0, None)?;
        let wanted = name.trim_start_matches('.');
        children
            .into_iter()
            .find(|c| c.name == wanted || c.name == format!("__{wanted}"))
            .map(|c| c.value)
            .ok_or_else(|| Error::Eval(format!("no member named `{wanted}`")))
    }

    /// Element `index` of an array, slice, vector or pointer.
    pub fn element(&mut self, value: &TypedValue, index: i128) -> Result<TypedValue> {
        let (_, def) = self
            .resolve(value.ty)
            .ok_or_else(|| Error::Eval("value cannot be indexed".into()))?;
        if index < 0 {
            return Err(Error::Eval("negative index".into()));
        }
        let index = index as u64;
        match &def.kind {
            TypeKind::Pointer { pointee, .. } => {
                let address = self.read_uint(&value.place, self.pointer_size())?;
                let size = pointee.and_then(|p| self.debug.type_size(p)).unwrap_or(1);
                Ok(TypedValue {
                    ty: *pointee,
                    place: Place::Memory(address.wrapping_add(index.wrapping_mul(size))),
                })
            }
            _ => {
                let children = self.children(value, index as usize, Some(1))?;
                children
                    .into_iter()
                    .next()
                    .map(|c| c.value)
                    .ok_or_else(|| Error::Eval(format!("index {index} is out of bounds")))
            }
        }
    }

    /// Writes a new value parsed from text into a memory-resident scalar.
    pub fn assign(&mut self, value: &TypedValue, text: &str) -> Result<()> {
        let Place::Memory(address) = value.place else {
            return Err(Error::Session(
                "this value is not stored in linear memory and cannot be modified".into(),
            ));
        };
        let (_, def) = self
            .resolve(value.ty)
            .ok_or_else(|| Error::Session("value has no known type".into()))?;
        let bytes = encode_scalar(&def, text, self.pointer_size())?;
        let instance = self
            .instance
            .ok_or_else(|| Error::Session("no instance to write memory to".into()))?;
        memory::write(self.store, instance, 0, address, &bytes)
    }
}

/// Total number of elements of a possibly multi-dimensional array.
fn element_count(counts: &[Option<u64>]) -> Option<u64> {
    counts
        .iter()
        .try_fold(1u64, |acc, c| c.and_then(|c| acc.checked_mul(c)))
}

fn not_a_container() -> Error {
    Error::Eval("`len()` applies to strings, slices, vectors and arrays".into())
}

fn visible_members(members: &[Member]) -> Vec<Member> {
    members
        .iter()
        .filter(|m| !m.artificial || m.name.is_some())
        .cloned()
        .collect()
}

fn base_scalar(encoding: BaseEncoding, bytes: &[u8]) -> Scalar {
    let size = bytes.len() as u8;
    match encoding {
        BaseEncoding::Float => Scalar::Float(match bytes.len() {
            4 => f64::from(f32::from_le_bytes(
                bytes[..4].try_into().unwrap_or_default(),
            )),
            8 => f64::from_le_bytes(bytes[..8].try_into().unwrap_or_default()),
            _ => f64::NAN,
        }),
        BaseEncoding::Boolean => Scalar::Bool(bytes.iter().any(|b| *b != 0)),
        BaseEncoding::Utf if bytes.len() == 4 => {
            let code = u32::from_le_bytes(bytes[..4].try_into().unwrap_or_default());
            char::from_u32(code)
                .map(Scalar::Char)
                .unwrap_or(Scalar::Int {
                    value: i128::from(code),
                    signed: false,
                    size,
                })
        }
        BaseEncoding::Signed | BaseEncoding::SignedChar => Scalar::Int {
            value: int_from_bytes(bytes, true),
            signed: true,
            size,
        },
        _ => Scalar::Int {
            value: int_from_bytes(bytes, false),
            signed: false,
            size,
        },
    }
}

pub(crate) fn int_from_bytes(bytes: &[u8], signed: bool) -> i128 {
    let mut buf = [0u8; 16];
    let n = bytes.len().min(16);
    buf[..n].copy_from_slice(&bytes[..n]);
    let value = u128::from_le_bytes(buf);
    if signed && n > 0 && n < 16 {
        let shift = 128 - n * 8;
        ((value << shift) as i128) >> shift
    } else {
        value as i128
    }
}

pub(crate) fn format_base(encoding: BaseEncoding, bytes: &[u8], hex: bool) -> String {
    if bytes.is_empty() {
        return "()".to_string();
    }
    match base_scalar(encoding, bytes) {
        Scalar::Bool(b) => b.to_string(),
        Scalar::Float(f) if bytes.len() == 4 => format!("{:?}", f as f32),
        Scalar::Float(f) => format!("{f:?}"),
        Scalar::Char(c) => format!("{c:?}"),
        Scalar::Int { value, size, .. } => {
            let number = if hex {
                let width = usize::from(size) * 2;
                let masked = (value as u128)
                    & if size >= 16 {
                        u128::MAX
                    } else {
                        (1u128 << (size * 8)) - 1
                    };
                format!("0x{masked:0width$x}")
            } else {
                value.to_string()
            };
            let is_char = matches!(
                encoding,
                BaseEncoding::SignedChar | BaseEncoding::UnsignedChar
            ) && size == 1;
            if is_char {
                let byte = value as u8;
                let shown = if byte.is_ascii_graphic() || byte == b' ' {
                    format!("'{}'", byte as char)
                } else {
                    format!("'\\x{byte:02x}'")
                };
                format!("{number} {shown}")
            } else {
                number
            }
        }
        Scalar::Pointer { address, .. } => format_address(address),
    }
}

pub(crate) fn format_address(address: u64) -> String {
    format!("0x{address:08x}")
}

fn bracket(items: &[String], total: usize) -> String {
    let more = if total > items.len() { ", …" } else { "" };
    format!("[{}{more}]", items.join(", "))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let parts: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("[{}]", parts.join(" "))
}

fn quote(text: &str) -> String {
    format!("{text:?}")
}

fn encode_scalar(def: &TypeDef, text: &str, pointer_size: u64) -> Result<Vec<u8>> {
    use crate::runtime::values::{parse_float, parse_int};
    let text = text.trim();
    let invalid = || Error::Session(format!("`{text}` is not a valid value for this variable"));
    let truncate = |value: i128, size: u64| value.to_le_bytes()[..size as usize].to_vec();
    match &def.kind {
        TypeKind::Base(encoding) => {
            let size = def.size.unwrap_or(4).min(16);
            match encoding {
                BaseEncoding::Float => {
                    let v = parse_float(text).ok_or_else(invalid)?;
                    Ok(if size == 4 {
                        (v as f32).to_le_bytes().to_vec()
                    } else {
                        v.to_le_bytes().to_vec()
                    })
                }
                BaseEncoding::Boolean => {
                    let v = match text {
                        "true" => 1,
                        "false" => 0,
                        other => parse_int(other).ok_or_else(invalid)?,
                    };
                    Ok(truncate(v, size))
                }
                BaseEncoding::Utf | BaseEncoding::SignedChar | BaseEncoding::UnsignedChar
                    if text.starts_with('\'') =>
                {
                    let c = text.trim_matches('\'').chars().next().ok_or_else(invalid)?;
                    Ok(truncate(i128::from(u32::from(c)), size))
                }
                _ => Ok(truncate(parse_int(text).ok_or_else(invalid)?, size)),
            }
        }
        TypeKind::Pointer { .. } => {
            Ok(truncate(parse_int(text).ok_or_else(invalid)?, pointer_size))
        }
        TypeKind::Enum { enumerators, .. } => {
            let size = def.size.unwrap_or(4).min(16);
            let value = enumerators
                .iter()
                .find(|(name, _)| name == text)
                .map(|(_, v)| *v)
                .or_else(|| parse_int(text))
                .ok_or_else(invalid)?;
            Ok(truncate(value, size))
        }
        _ => Err(Error::Session(
            "only numbers, booleans, characters, enums and pointers can be modified".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_sign_extend_by_width() {
        assert_eq!(int_from_bytes(&[0xff], true), -1);
        assert_eq!(int_from_bytes(&[0xff], false), 255);
        assert_eq!(int_from_bytes(&[0xfe, 0xff, 0xff, 0xff], true), -2);
    }

    #[test]
    fn base_values_format_by_encoding() {
        assert_eq!(
            format_base(BaseEncoding::Signed, &(-5i32).to_le_bytes(), false),
            "-5"
        );
        assert_eq!(
            format_base(BaseEncoding::Unsigned, &255u32.to_le_bytes(), true),
            "0x000000ff"
        );
        assert_eq!(format_base(BaseEncoding::Boolean, &[1], false), "true");
        assert_eq!(
            format_base(BaseEncoding::Float, &0.5f64.to_le_bytes(), false),
            "0.5"
        );
        assert_eq!(
            format_base(BaseEncoding::Utf, &u32::from('w').to_le_bytes(), false),
            "'w'"
        );
        assert_eq!(format_base(BaseEncoding::SignedChar, b"A", false), "65 'A'");
    }
}
