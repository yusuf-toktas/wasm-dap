use std::sync::Arc;

use wasm_dap_core::dwarf::{DebugInfo, TypeKind, VariableLocation};
use wasm_dap_core::module::WasmModule;
use wasm_dap_testkit::{RustTarget, line_of, rust_fixture};

fn load_arith() -> (Arc<WasmModule>, DebugInfo) {
    let path = rust_fixture("arith.rs", RustTarget::Library);
    let bytes = std::fs::read(&path).unwrap();
    let module = Arc::new(WasmModule::parse(bytes).unwrap());
    let debug = DebugInfo::load(module.bytes())
        .unwrap()
        .expect("fixture has DWARF");
    (module, debug)
}

#[test]
fn source_files_are_joined_with_comp_dir() {
    let (_, debug) = load_arith();
    let files = debug.lines().find_files("/fixtures/arith.rs");
    assert_eq!(files.len(), 1, "files: {:?}", debug.lines().files());
    assert_eq!(
        debug.lines().file_path(files[0]),
        Some("/fixtures/arith.rs")
    );
}

#[test]
fn line_breakpoint_resolves_inside_the_right_function() {
    let (module, debug) = load_arith();
    let files = debug.lines().find_files("/fixtures/arith.rs");
    let line = line_of("arith.rs", "BP: fib-b");
    let resolved = debug.lines().resolve_line(&files, line).unwrap();
    assert_eq!(resolved.line, line);
    assert_eq!(resolved.addresses.len(), 1);
    let address = resolved.addresses[0];
    let function = module
        .function_at(address)
        .expect("address inside a function");
    assert_eq!(module.display_function_name(function.index), "fib");
    assert!(
        module.is_instruction(address),
        "breakpoint must sit on an opcode"
    );
    assert_eq!(debug.lines().location(address).unwrap().line, line);
    assert_eq!(debug.subprogram_at(address).unwrap().name, "arith::fib");
}

#[test]
fn breakpoint_on_line_without_code_slides_forward() {
    let (_, debug) = load_arith();
    let files = debug.lines().find_files("/fixtures/arith.rs");
    let blank = line_of("arith.rs", "pub struct Point") - 1;
    let resolved = debug.lines().resolve_line(&files, blank).unwrap();
    assert!(resolved.line > blank);
}

#[test]
fn frame_variables_include_parameters_and_scoped_locals() {
    let (_, debug) = load_arith();
    let files = debug.lines().find_files("/fixtures/arith.rs");
    let line = line_of("arith.rs", "BP: fib-sum");
    let address = debug.lines().resolve_line(&files, line).unwrap().addresses[0];
    let frame = debug.frame_variables(address).expect("frame variables");
    let names: Vec<&str> = frame.variables.iter().map(|v| v.name.as_str()).collect();
    assert!(names.contains(&"n"), "{names:?}");
    assert!(names.contains(&"a"), "{names:?}");
    assert!(names.contains(&"b"), "{names:?}");
    assert!(frame.frame_base.is_some());
    let n = frame.variables.iter().find(|v| v.name == "n").unwrap();
    assert!(n.parameter);
    assert!(matches!(n.location, VariableLocation::Expr(_)));
    assert_eq!(debug.type_name(n.ty.unwrap()), "u32");
}

#[test]
fn composite_types_are_modelled() {
    let (_, debug) = load_arith();
    let files = debug.lines().find_files("/fixtures/arith.rs");
    let line = line_of("arith.rs", "BP: sum-loop");
    let address = debug.lines().resolve_line(&files, line).unwrap().addresses[0];
    let frame = debug.frame_variables(address).unwrap();
    let points = frame.variables.iter().find(|v| v.name == "points").unwrap();
    let ty = points.ty.unwrap();
    assert_eq!(
        debug.type_name(ty),
        "[arith::Point; 3]".replace("arith::", "")
    );
    let (_, def) = debug.resolve_type(ty).unwrap();
    match &def.kind {
        TypeKind::Array { element, counts } => {
            assert_eq!(counts, &vec![Some(3)]);
            let (_, point) = debug.resolve_type(element.unwrap()).unwrap();
            match &point.kind {
                TypeKind::Struct { members, .. } => {
                    let names: Vec<_> = members.iter().filter_map(|m| m.name.clone()).collect();
                    assert_eq!(names, vec!["x", "y"]);
                    assert_eq!(members[1].offset, 4);
                }
                other => panic!("expected struct, got {other:?}"),
            }
        }
        other => panic!("expected array, got {other:?}"),
    }
    assert_eq!(debug.type_size(ty), Some(24));
}

#[test]
fn rust_enums_expose_variant_parts() {
    let (_, debug) = load_arith();
    let files = debug.lines().find_files("/fixtures/arith.rs");
    let line = line_of("arith.rs", "BP: shapes-total");
    let address = debug.lines().resolve_line(&files, line).unwrap().addresses[0];
    let frame = debug.frame_variables(address).unwrap();
    let rect = frame.variables.iter().find(|v| v.name == "rect").unwrap();
    let (_, def) = debug.resolve_type(rect.ty.unwrap()).unwrap();
    match &def.kind {
        TypeKind::Struct {
            variant_part: Some(part),
            ..
        } => {
            assert!(part.discriminant.is_some());
            assert_eq!(part.variants.len(), 3);
        }
        other => panic!("expected enum with variant part, got {other:?}"),
    }
}

/// C compilers default to DWARF 5 (line table v5, string offsets, address
/// tables, range lists); rustc can emit it too.
#[test]
fn dwarf5_output_is_understood() {
    let path = wasm_dap_testkit::rust_fixture_with(
        "arith.rs",
        RustTarget::Library,
        0,
        &["-C", "dwarf-version=5"],
    );
    let module = Arc::new(WasmModule::parse(std::fs::read(&path).unwrap()).unwrap());
    let debug = DebugInfo::load(module.bytes())
        .unwrap()
        .expect("DWARF present");

    let files = debug.lines().find_files("/fixtures/arith.rs");
    assert_eq!(files.len(), 1, "files: {:?}", debug.lines().files());
    let line = line_of("arith.rs", "BP: fib-sum");
    let resolved = debug.lines().resolve_line(&files, line).unwrap();
    assert_eq!(resolved.line, line);
    let address = resolved.addresses[0];
    let function = module.function_at(address).unwrap();
    assert_eq!(module.display_function_name(function.index), "fib");
    assert_eq!(debug.subprogram_at(address).unwrap().name, "arith::fib");

    let frame = debug.frame_variables(address).expect("frame variables");
    let names: Vec<&str> = frame.variables.iter().map(|v| v.name.as_str()).collect();
    for expected in ["n", "a", "b"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    let n = frame.variables.iter().find(|v| v.name == "n").unwrap();
    assert_eq!(debug.type_name(n.ty.unwrap()), "u32");

    let loop_line = line_of("arith.rs", "BP: sum-loop");
    let address = debug
        .lines()
        .resolve_line(&files, loop_line)
        .unwrap()
        .addresses[0];
    let frame = debug.frame_variables(address).unwrap();
    let points = frame.variables.iter().find(|v| v.name == "points").unwrap();
    assert_eq!(debug.type_size(points.ty.unwrap()), Some(24));
}
