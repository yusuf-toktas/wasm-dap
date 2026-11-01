//! Evaluation of debugger expressions against the debuggee.

use wasmtime::{AsContextMut, Func, Instance, StoreContextMut, Val, ValType};

use super::frames::{self, FrameAccess};
use super::program::ProgramInfo;
use super::values::{Place, Scalar, TypedValue, ValueReader, int_from_bytes};
use super::variables::Paused;
use crate::error::{Error, Result};
use crate::evaluator::expression::{BinaryOp, Expr, UnaryOp};
use crate::runtime::HostState;
use crate::runtime::values::format_val;

/// Runs a WebAssembly function on behalf of the evaluator while paused.
pub(crate) trait NestedCaller {
    fn call(
        &self,
        store: &mut StoreContextMut<'_, HostState>,
        func: Func,
        params: &[Val],
        results: &mut [Val],
    ) -> Result<()>;
}

#[derive(Debug, Clone)]
pub(crate) enum Value {
    Scalar(Scalar),
    Typed(TypedValue),
    Wasm(Val),
    Str(String),
    Void,
}

pub(crate) struct Evaluator<'s, 'a> {
    pub store: &'s mut StoreContextMut<'a, HostState>,
    pub info: &'s ProgramInfo,
    pub paused: &'s Paused,
    pub frame: Option<usize>,
    pub instance: Option<Instance>,
    pub caller: &'s dyn NestedCaller,
    /// False for hover evaluation, which must not run debuggee code.
    pub allow_calls: bool,
}

impl<'s, 'a> Evaluator<'s, 'a> {
    pub fn evaluate(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Integer(v) => Ok(Value::Scalar(Scalar::Int {
                value: *v,
                signed: true,
                size: 8,
            })),
            Expr::Float(v) => Ok(Value::Scalar(Scalar::Float(*v))),
            Expr::Bool(v) => Ok(Value::Scalar(Scalar::Bool(*v))),
            Expr::Char(c) => Ok(Value::Scalar(Scalar::Char(*c))),
            Expr::String(s) => Ok(Value::Str(s.clone())),
            Expr::Identifier(name) => self.lookup(name),
            Expr::Call { name, args } => self.call(name, args),
            Expr::Unary { op, expr } => self.unary(*op, expr),
            Expr::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs),
            Expr::Member { expr, field, arrow } => {
                let base = self.evaluate(expr)?;
                let mut typed = self.typed(base)?;
                if *arrow || self.is_pointer(&typed) {
                    typed = self.deref(typed)?;
                }
                self.reader()?.member(&typed, field).map(Value::Typed)
            }
            Expr::MethodCall {
                receiver,
                method,
                args,
            } => {
                if !matches!(method.as_str(), "len" | "is_empty") {
                    return Err(Error::Eval(format!(
                        "unsupported method `{method}()`; available: len(), is_empty()"
                    )));
                }
                if !args.is_empty() {
                    return Err(Error::Eval(format!("`{method}()` takes no arguments")));
                }
                let base = self.evaluate(receiver)?;
                let typed = self.typed(base)?;
                let length = self.reader()?.length(&typed)?;
                Ok(Value::Scalar(if method == "len" {
                    Scalar::Int {
                        value: i128::from(length),
                        signed: false,
                        size: 4,
                    }
                } else {
                    Scalar::Bool(length == 0)
                }))
            }
            Expr::Index { expr, index } => {
                let base = self.evaluate(expr)?;
                let index = self.evaluate(index)?;
                let index = match self.scalar(index)? {
                    Scalar::Int { value, .. } => value,
                    _ => return Err(Error::Eval("index must be an integer".into())),
                };
                let typed = self.typed(base)?;
                self.reader()?.element(&typed, index).map(Value::Typed)
            }
        }
    }

    pub fn truthy(&mut self, value: Value) -> Result<bool> {
        Ok(match value {
            Value::Str(s) => !s.is_empty(),
            Value::Void => false,
            other => match self.scalar(other)? {
                Scalar::Int { value, .. } => value != 0,
                Scalar::Float(f) => f != 0.0,
                Scalar::Bool(b) => b,
                Scalar::Char(c) => c != '\0',
                Scalar::Pointer { address, .. } => address != 0,
            },
        })
    }

    fn reader(&mut self) -> Result<ValueReader<'_, 'a>> {
        let debug = self
            .info
            .debug
            .as_ref()
            .ok_or_else(|| Error::Eval("the module has no debug information".into()))?;
        Ok(ValueReader {
            store: self.store,
            instance: self.instance,
            debug,
        })
    }

    fn frame_data(&self) -> Option<&frames::FrameData> {
        self.paused.frames.get(self.frame?)
    }

    pub fn scalar(&mut self, value: Value) -> Result<Scalar> {
        match value {
            Value::Scalar(s) => Ok(s),
            Value::Typed(t) => self.reader()?.scalar(&t),
            Value::Wasm(val) => match val {
                Val::I32(v) => Ok(Scalar::Int {
                    value: i128::from(v),
                    signed: true,
                    size: 4,
                }),
                Val::I64(v) => Ok(Scalar::Int {
                    value: i128::from(v),
                    signed: true,
                    size: 8,
                }),
                Val::F32(bits) => Ok(Scalar::Float(f64::from(f32::from_bits(bits)))),
                Val::F64(bits) => Ok(Scalar::Float(f64::from_bits(bits))),
                Val::V128(v) => Ok(Scalar::Int {
                    value: v.as_u128() as i128,
                    signed: false,
                    size: 16,
                }),
                other => Err(Error::Eval(format!(
                    "{} values cannot be used in arithmetic",
                    crate::runtime::values::val_type_name(&other)
                ))),
            },
            Value::Str(_) => Err(Error::Eval("strings cannot be used as numbers".into())),
            Value::Void => Err(Error::Eval("the expression has no value".into())),
        }
    }

    /// Source-level value of an operand; pointer scalars designate their pointee.
    fn typed(&mut self, value: Value) -> Result<TypedValue> {
        match value {
            Value::Typed(t) => Ok(t),
            Value::Scalar(Scalar::Pointer {
                address,
                pointee: Some(ty),
            }) => Ok(TypedValue {
                ty: Some(ty),
                place: Place::Memory(address),
            }),
            _ => Err(Error::Eval(
                "this operation needs a source-level variable".into(),
            )),
        }
    }

    fn is_pointer(&self, value: &TypedValue) -> bool {
        self.info
            .debug
            .as_ref()
            .and_then(|d| value.ty.and_then(|t| d.resolve_type(t)))
            .is_some_and(|(_, def)| matches!(def.kind, crate::dwarf::TypeKind::Pointer { .. }))
    }

    fn deref(&mut self, value: TypedValue) -> Result<TypedValue> {
        match self.reader()?.scalar(&value)? {
            Scalar::Pointer { address, pointee } => {
                if address == 0 {
                    return Err(Error::Eval("null pointer dereference".into()));
                }
                Ok(TypedValue {
                    ty: pointee,
                    place: Place::Memory(address),
                })
            }
            _ => Err(Error::Eval("only pointers can be dereferenced".into())),
        }
    }

    fn lookup(&mut self, name: &str) -> Result<Value> {
        if let Some(rest) = name.strip_prefix('$') {
            return self.wasm_reference(rest);
        }
        if let Some(value) = self.dwarf_local(name) {
            return Ok(value);
        }
        if let Some(value) = self.dwarf_static(name) {
            return Ok(value);
        }
        if let Some(index) = self.info.module.find_global(name) {
            return self.global(index);
        }
        Err(Error::Eval(format!("unknown identifier `{name}`")))
    }

    fn dwarf_local(&mut self, name: &str) -> Option<Value> {
        let debug = self.info.debug.clone()?;
        let index = self.frame?;
        let frame = self.paused.frames.get(index)?;
        let scope = debug.frame_variables(frame.pc?)?;
        let instance = self.instance;
        frames::locate_variables(self.store, &debug, frame, instance, &scope)
            .into_iter()
            .rev()
            .find(|(v, _)| v.name == name)
            .map(|(v, place)| Value::Typed(TypedValue { ty: v.ty, place }))
    }

    fn dwarf_static(&mut self, name: &str) -> Option<Value> {
        let debug = self.info.debug.clone()?;
        let variable = debug.global_variables(name).into_iter().next()?;
        let encoding = gimli::Encoding {
            address_size: debug.address_size(),
            format: gimli::Format::Dwarf32,
            version: 4,
        };
        let mut access = FrameAccess {
            store: self.store,
            frame: None,
            instance: self.instance,
            frame_base: None,
        };
        let size = variable.ty.and_then(|t| debug.type_size(t));
        let place = frames::locate(&variable, 0, encoding, size, &mut access);
        let place = match (&variable.location, place) {
            (crate::dwarf::VariableLocation::Expr(_), Place::Unavailable(_)) => {
                let expression = variable.expression_at(0)?;
                match crate::dwarf::expr::evaluate(expression, encoding, &mut access).ok()? {
                    crate::dwarf::Location::Memory(address) => Place::Memory(address),
                    _ => return None,
                }
            }
            (_, place) => place,
        };
        Some(Value::Typed(TypedValue {
            ty: variable.ty,
            place,
        }))
    }

    fn global(&mut self, index: u32) -> Result<Value> {
        let instance = self
            .instance
            .ok_or_else(|| Error::Eval("the module is not instantiated".into()))?;
        frames::global(self.store, instance, index)
            .map(Value::Wasm)
            .ok_or_else(|| Error::Eval(format!("global {index} is not accessible")))
    }

    fn wasm_reference(&mut self, rest: &str) -> Result<Value> {
        let number = |prefix: &str| {
            rest.strip_prefix(prefix)
                .and_then(|n| n.parse::<u32>().ok())
        };
        if let Some(index) = number("local").or_else(|| number("param")) {
            let handle = self
                .frame_data()
                .map(|f| f.handle.clone())
                .ok_or_else(|| Error::Eval("no frame is selected".into()))?;
            return frames::local(self.store, &handle, index)
                .map(Value::Wasm)
                .ok_or_else(|| Error::Eval(format!("local {index} does not exist")));
        }
        if let Some(index) = number("stack") {
            let handle = self
                .frame_data()
                .map(|f| f.handle.clone())
                .ok_or_else(|| Error::Eval("no frame is selected".into()))?;
            return frames::stack(self.store, &handle, index)
                .map(Value::Wasm)
                .ok_or_else(|| Error::Eval(format!("operand stack slot {index} does not exist")));
        }
        if let Some(index) = number("global") {
            return self.global(index);
        }
        if rest == "pc" {
            let pc = self
                .frame_data()
                .and_then(|f| f.pc)
                .ok_or_else(|| Error::Eval("no frame is selected".into()))?;
            return Ok(Value::Scalar(Scalar::Int {
                value: i128::from(pc),
                signed: false,
                size: 4,
            }));
        }
        if let Some(frame) = self.frame_data()
            && let Some(function) = frame.pc.and_then(|pc| self.info.module.function_at(pc))
        {
            let index = function.index;
            let names = self.info.module.names.locals.get(&index);
            if let Some(local) =
                names.and_then(|m| m.iter().find(|(_, n)| n.as_str() == rest).map(|(i, _)| *i))
            {
                let handle = frame.handle.clone();
                return frames::local(self.store, &handle, local)
                    .map(Value::Wasm)
                    .ok_or_else(|| Error::Eval(format!("local `{rest}` is not available")));
            }
        }
        if let Some(index) = self.info.module.find_global(rest) {
            return self.global(index);
        }
        Err(Error::Eval(format!(
            "unknown WebAssembly reference `${rest}`"
        )))
    }

    fn unary(&mut self, op: UnaryOp, operand: &Expr) -> Result<Value> {
        let value = self.evaluate(operand)?;
        match op {
            UnaryOp::Deref => match value {
                Value::Typed(typed) => self.deref(typed).map(Value::Typed),
                pointer @ Value::Scalar(Scalar::Pointer { .. }) => {
                    self.typed(pointer).map(Value::Typed)
                }
                _ => Err(Error::Eval("only pointers can be dereferenced".into())),
            },
            UnaryOp::AddressOf => match value {
                Value::Typed(TypedValue {
                    ty,
                    place: Place::Memory(address),
                }) => Ok(Value::Scalar(Scalar::Pointer {
                    address,
                    pointee: ty,
                })),
                _ => Err(Error::Eval(
                    "only values in linear memory have an address".into(),
                )),
            },
            UnaryOp::Not => Ok(Value::Scalar(Scalar::Bool(!self.truthy(value)?))),
            UnaryOp::Neg => match self.scalar(value)? {
                Scalar::Int { value, size, .. } => Ok(Value::Scalar(Scalar::Int {
                    value: value.wrapping_neg(),
                    signed: true,
                    size,
                })),
                Scalar::Float(f) => Ok(Value::Scalar(Scalar::Float(-f))),
                _ => Err(Error::Eval("operand cannot be negated".into())),
            },
            UnaryOp::BitNot => match self.scalar(value)? {
                Scalar::Int {
                    value,
                    signed,
                    size,
                } => Ok(Value::Scalar(Scalar::Int {
                    value: wrap(!value, signed, size),
                    signed,
                    size,
                })),
                _ => Err(Error::Eval("`~` needs an integer".into())),
            },
        }
    }

    fn binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Result<Value> {
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            let left = self.evaluate(lhs)?;
            let left = self.truthy(left)?;
            if (op == BinaryOp::And && !left) || (op == BinaryOp::Or && left) {
                return Ok(Value::Scalar(Scalar::Bool(left)));
            }
            let right = self.evaluate(rhs)?;
            return Ok(Value::Scalar(Scalar::Bool(self.truthy(right)?)));
        }
        let left = self.evaluate(lhs)?;
        let right = self.evaluate(rhs)?;
        let a = self.scalar(left)?;
        let b = self.scalar(right)?;
        self.arithmetic(op, a, b).map(Value::Scalar)
    }

    fn element_size(&self, pointee: Option<crate::dwarf::TypeId>) -> u64 {
        self.info
            .debug
            .as_ref()
            .and_then(|d| pointee.and_then(|p| d.type_size(p)))
            .unwrap_or(1)
            .max(1)
    }

    fn arithmetic(&self, op: BinaryOp, a: Scalar, b: Scalar) -> Result<Scalar> {
        if let (Scalar::Pointer { address, pointee }, Scalar::Int { value, .. }) = (&a, &b)
            && matches!(op, BinaryOp::Add | BinaryOp::Sub)
        {
            let delta = (*value as i64).wrapping_mul(self.element_size(*pointee) as i64) as u64;
            let address = if op == BinaryOp::Add {
                address.wrapping_add(delta)
            } else {
                address.wrapping_sub(delta)
            };
            return Ok(Scalar::Pointer {
                address,
                pointee: *pointee,
            });
        }
        let as_float = |s: &Scalar| match s {
            Scalar::Float(f) => Some(*f),
            _ => None,
        };
        if as_float(&a).is_some() || as_float(&b).is_some() {
            let x = to_f64(&a);
            let y = to_f64(&b);
            return Ok(match op {
                BinaryOp::Add => Scalar::Float(x + y),
                BinaryOp::Sub => Scalar::Float(x - y),
                BinaryOp::Mul => Scalar::Float(x * y),
                BinaryOp::Div => Scalar::Float(x / y),
                BinaryOp::Rem => Scalar::Float(x % y),
                BinaryOp::Lt => Scalar::Bool(x < y),
                BinaryOp::Le => Scalar::Bool(x <= y),
                BinaryOp::Gt => Scalar::Bool(x > y),
                BinaryOp::Ge => Scalar::Bool(x >= y),
                BinaryOp::Eq => Scalar::Bool(x == y),
                BinaryOp::Ne => Scalar::Bool(x != y),
                _ => return Err(Error::Eval("bitwise operators need integers".into())),
            });
        }
        let (x, xs, xsize) = to_int(&a);
        let (y, ys, ysize) = to_int(&b);
        let signed = xs || ys;
        let size = xsize.max(ysize);
        let int = |value: i128| Scalar::Int {
            value: wrap(value, signed, size),
            signed,
            size,
        };
        Ok(match op {
            BinaryOp::Add => int(x.wrapping_add(y)),
            BinaryOp::Sub => int(x.wrapping_sub(y)),
            BinaryOp::Mul => int(x.wrapping_mul(y)),
            BinaryOp::Div => {
                if y == 0 {
                    return Err(Error::Eval("division by zero".into()));
                }
                int(x.wrapping_div(y))
            }
            BinaryOp::Rem => {
                if y == 0 {
                    return Err(Error::Eval("division by zero".into()));
                }
                int(x.wrapping_rem(y))
            }
            BinaryOp::Shl => int(x.wrapping_shl(y as u32)),
            BinaryOp::Shr => int(x.wrapping_shr(y as u32)),
            BinaryOp::BitAnd => int(x & y),
            BinaryOp::BitOr => int(x | y),
            BinaryOp::BitXor => int(x ^ y),
            BinaryOp::Lt => Scalar::Bool(x < y),
            BinaryOp::Le => Scalar::Bool(x <= y),
            BinaryOp::Gt => Scalar::Bool(x > y),
            BinaryOp::Ge => Scalar::Bool(x >= y),
            BinaryOp::Eq => Scalar::Bool(x == y),
            BinaryOp::Ne => Scalar::Bool(x != y),
            BinaryOp::And | BinaryOp::Or => unreachable!(),
        })
    }

    fn call(&mut self, name: &str, args: &[Expr]) -> Result<Value> {
        if let Some(builtin) = name.strip_prefix('$') {
            return self.builtin(builtin, args);
        }
        if !self.allow_calls {
            return Err(Error::Eval(
                "function calls are not evaluated on hover".into(),
            ));
        }
        let index = self
            .resolve_function(name)
            .ok_or_else(|| Error::Eval(format!("unknown function `{name}`")))?;
        let instance = self
            .instance
            .ok_or_else(|| Error::Eval("the module is not instantiated yet".into()))?;
        let func = instance
            .debug_function(self.store.as_context_mut(), index)
            .ok_or_else(|| Error::Eval(format!("function `{name}` is not callable")))?;
        let ty = func.ty(self.store.as_context_mut());
        let params: Vec<ValType> = ty.params().collect();
        if params.len() != args.len() {
            return Err(Error::Eval(format!(
                "`{name}` expects {} argument(s), got {}",
                params.len(),
                args.len()
            )));
        }
        let mut values = Vec::with_capacity(args.len());
        for (param, arg) in params.iter().zip(args) {
            let value = self.evaluate(arg)?;
            let scalar = self.scalar(value)?;
            values.push(scalar_to_val(&scalar, param)?);
        }
        let mut results: Vec<Val> = ty
            .results()
            .map(|r| Val::default_for_ty(&r).unwrap_or(Val::I32(0)))
            .collect();
        self.caller.call(self.store, func, &values, &mut results)?;
        Ok(match results.into_iter().next() {
            Some(value) => Value::Wasm(value),
            None => Value::Void,
        })
    }

    fn resolve_function(&self, name: &str) -> Option<u32> {
        if let Some(index) = self.info.module.exported_function(name) {
            return Some(index);
        }
        if let Some(index) = self.info.module.find_functions(name).first() {
            return Some(*index);
        }
        let debug = self.info.debug.as_ref()?;
        debug
            .find_subprograms(name)
            .first()
            .and_then(|s| self.info.module.function_at(s.low))
            .map(|f| f.index)
    }

    fn builtin(&mut self, name: &str, args: &[Expr]) -> Result<Value> {
        let address = match args.first() {
            Some(arg) => {
                let value = self.evaluate(arg)?;
                match self.scalar(value)? {
                    Scalar::Int { value, .. } => {
                        u64::try_from(value).map_err(|_| Error::Eval("negative address".into()))?
                    }
                    Scalar::Pointer { address, .. } => address,
                    _ => return Err(Error::Eval("address must be an integer".into())),
                }
            }
            None => return Err(Error::Eval(format!("${name} needs an address argument"))),
        };
        let instance = self
            .instance
            .ok_or_else(|| Error::Eval("the module is not instantiated".into()))?;
        let (size, signed, float) = match name {
            "i8" => (1, true, false),
            "u8" => (1, false, false),
            "i16" => (2, true, false),
            "u16" => (2, false, false),
            "i32" => (4, true, false),
            "u32" => (4, false, false),
            "i64" => (8, true, false),
            "u64" => (8, false, false),
            "f32" => (4, true, true),
            "f64" => (8, true, true),
            "str" => {
                let limit = match args.get(1) {
                    Some(arg) => {
                        let value = self.evaluate(arg)?;
                        match self.scalar(value)? {
                            Scalar::Int { value, .. } => value.clamp(0, 65536) as usize,
                            _ => 256,
                        }
                    }
                    None => 256,
                };
                let bytes = super::memory::read(self.store, instance, 0, address, limit)?;
                let end = if args.len() > 1 {
                    bytes.len()
                } else {
                    bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())
                };
                return Ok(Value::Str(
                    String::from_utf8_lossy(&bytes[..end]).into_owned(),
                ));
            }
            other => return Err(Error::Eval(format!("unknown builtin `${other}`"))),
        };
        let bytes = super::memory::read_exact(self.store, instance, address, size)?;
        if float {
            let value = if size == 4 {
                f64::from(f32::from_le_bytes(
                    bytes[..4].try_into().unwrap_or_default(),
                ))
            } else {
                f64::from_le_bytes(bytes[..8].try_into().unwrap_or_default())
            };
            return Ok(Value::Scalar(Scalar::Float(value)));
        }
        Ok(Value::Scalar(Scalar::Int {
            value: int_from_bytes(&bytes, signed),
            signed,
            size: size as u8,
        }))
    }
}

fn to_f64(s: &Scalar) -> f64 {
    match s {
        Scalar::Float(f) => *f,
        Scalar::Int { value, .. } => *value as f64,
        Scalar::Bool(b) => f64::from(u8::from(*b)),
        Scalar::Char(c) => f64::from(u32::from(*c)),
        Scalar::Pointer { address, .. } => *address as f64,
    }
}

fn to_int(s: &Scalar) -> (i128, bool, u8) {
    match s {
        Scalar::Int {
            value,
            signed,
            size,
        } => (*value, *signed, *size),
        Scalar::Bool(b) => (i128::from(*b), false, 1),
        Scalar::Char(c) => (i128::from(u32::from(*c)), false, 4),
        Scalar::Pointer { address, .. } => (i128::from(*address), false, 4),
        Scalar::Float(f) => (*f as i128, true, 8),
    }
}

fn wrap(value: i128, signed: bool, size: u8) -> i128 {
    if size == 0 || size >= 16 {
        return value;
    }
    let bits = u32::from(size) * 8;
    let mask = (1i128 << bits) - 1;
    let truncated = value & mask;
    if signed && truncated >> (bits - 1) & 1 == 1 {
        truncated - (1i128 << bits)
    } else {
        truncated
    }
}

fn scalar_to_val(scalar: &Scalar, ty: &ValType) -> Result<Val> {
    let int = || match scalar {
        Scalar::Int { value, .. } => Some(*value),
        Scalar::Bool(b) => Some(i128::from(*b)),
        Scalar::Char(c) => Some(i128::from(u32::from(*c))),
        Scalar::Pointer { address, .. } => Some(i128::from(*address)),
        Scalar::Float(_) => None,
    };
    match ty {
        ValType::I32 => int()
            .map(|v| Val::I32(v as i32))
            .ok_or_else(|| Error::Eval("expected an integer argument".into())),
        ValType::I64 => int()
            .map(|v| Val::I64(v as i64))
            .ok_or_else(|| Error::Eval("expected an integer argument".into())),
        ValType::F32 => Ok(Val::F32((to_f64(scalar) as f32).to_bits())),
        ValType::F64 => Ok(Val::F64(to_f64(scalar).to_bits())),
        other => Err(Error::Eval(format!("cannot pass a value of type {other}"))),
    }
}

/// Short text for a scalar or WebAssembly value.
pub(crate) fn describe_simple(value: &Value, hex: bool) -> (String, Option<String>) {
    match value {
        Value::Scalar(Scalar::Int {
            value,
            signed,
            size,
        }) => {
            let text = if hex {
                let width = usize::from(*size) * 2;
                let mask = if *size >= 16 {
                    u128::MAX
                } else {
                    (1u128 << (size * 8)) - 1
                };
                format!("0x{:0width$x}", (*value as u128) & mask)
            } else {
                value.to_string()
            };
            let ty = format!(
                "{}{}",
                if *signed { "i" } else { "u" },
                u32::from(*size) * 8
            );
            (text, Some(ty))
        }
        Value::Scalar(Scalar::Float(f)) => (format!("{f:?}"), Some("f64".into())),
        Value::Scalar(Scalar::Bool(b)) => (b.to_string(), Some("bool".into())),
        Value::Scalar(Scalar::Char(c)) => (format!("{c:?}"), Some("char".into())),
        Value::Scalar(Scalar::Pointer { address, .. }) => (
            super::values::format_address(*address),
            Some("pointer".into()),
        ),
        Value::Wasm(val) => (
            format_val(val, hex),
            Some(crate::runtime::values::val_type_name(val).into()),
        ),
        Value::Str(s) => (format!("{s:?}"), Some("string".into())),
        Value::Void => ("void".into(), None),
        Value::Typed(_) => (String::new(), None),
    }
}
