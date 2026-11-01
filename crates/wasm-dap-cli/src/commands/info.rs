use std::process::ExitCode;

use anyhow::Result;
use serde_json::{Value, json};
use wasm_dap_core::dwarf::DebugInfo;
use wasm_dap_core::module::{ExternKind, WasmModule};

use super::load_static;
use crate::cli::InfoArgs;

pub fn run(args: InfoArgs) -> Result<ExitCode> {
    let (module, debug) = load_static(&args.file)?;
    let size = module.bytes().len();
    if args.json {
        let report = report(&module, debug.as_ref(), &args.file.to_string_lossy(), size);
        outln!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&module, debug.as_ref(), &args.file.to_string_lossy(), size);
    }
    Ok(ExitCode::SUCCESS)
}

fn function_location(module: &WasmModule, debug: Option<&DebugInfo>, index: u32) -> Option<String> {
    let debug = debug?;
    let first = module.function(index)?.body.as_ref()?.first_instruction()?;
    let line = debug.lines().location(first)?;
    let path = debug.lines().file_path(line.file)?;
    Some(format!("{path}:{}", line.line))
}

fn function_name(module: &WasmModule, debug: Option<&DebugInfo>, index: u32) -> String {
    let dwarf = module
        .function(index)
        .and_then(|f| f.body.as_ref())
        .and_then(|b| debug.and_then(|d| d.subprogram_at(b.range.start)))
        .map(|s| s.name.clone());
    dwarf.unwrap_or_else(|| module.display_function_name(index))
}

fn debug_summary(module: &WasmModule, debug: Option<&DebugInfo>) -> String {
    match debug {
        Some(d) => format!(
            "DWARF ({} compilation units, {} source files, {} functions)",
            d.unit_count(),
            d.lines().files_with_code().len(),
            d.subprograms().len()
        ),
        None if !module.names.functions.is_empty() => "name section only (no DWARF)".into(),
        None => "none".into(),
    }
}

fn print_human(module: &WasmModule, debug: Option<&DebugInfo>, path: &str, size: usize) {
    outln!("Module    : {path} ({size} bytes)");
    if let Some(name) = &module.names.module {
        outln!("Name      : {name}");
    }
    for field in &module.producers {
        let values: Vec<String> = field
            .values
            .iter()
            .map(|(n, v)| {
                if v.is_empty() {
                    n.clone()
                } else {
                    format!("{n} {v}")
                }
            })
            .collect();
        outln!("Producer  : {}: {}", field.name, values.join(", "));
    }
    outln!("Debug info: {}", debug_summary(module, debug));
    if let Some(start) = module.start {
        outln!("Start     : {}", module.display_function_name(start));
    }

    let imported = module.imported_function_count();
    let defined = module.functions.len() as u32 - imported;
    outln!();
    outln!("Types     : {}", module.types.len());
    outln!(
        "Functions : {} ({imported} imported, {defined} defined)",
        module.functions.len()
    );

    if !module.imports.is_empty() {
        outln!();
        outln!("Imports ({}):", module.imports.len());
        for import in &module.imports {
            let detail = match import.kind {
                ExternKind::Func => module
                    .func_sig(import.index)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            outln!(
                "  {:<7} {}.{} {detail}",
                import.kind.as_str(),
                import.module,
                import.name
            );
        }
    }

    if !module.exports.is_empty() {
        outln!();
        outln!("Exports ({}):", module.exports.len());
        for export in &module.exports {
            let detail = match export.kind {
                ExternKind::Func => format!(
                    "{} {}",
                    function_name(module, debug, export.index),
                    module
                        .func_sig(export.index)
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                ),
                _ => format!("index {}", export.index),
            };
            outln!("  {:<7} {:<24} {detail}", export.kind.as_str(), export.name);
        }
    }

    if defined > 0 {
        outln!();
        outln!("Defined functions ({defined}):");
        for function in module.defined_functions() {
            let body = function.body.as_ref().expect("defined function has a body");
            let sig = module
                .func_sig(function.index)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let location = function_location(module, debug, function.index)
                .map(|l| format!("  {l}"))
                .unwrap_or_default();
            outln!(
                "  [{:>4}] {} {sig}  ({} bytes, {} instructions){location}",
                function.index,
                function_name(module, debug, function.index),
                body.range.end - body.range.start,
                body.instructions.len()
            );
        }
    }

    if !module.memories.is_empty() {
        outln!();
        outln!("Memories ({}):", module.memories.len());
        for (i, memory) in module.memories.iter().enumerate() {
            let max = memory
                .maximum
                .map(|m| m.to_string())
                .unwrap_or_else(|| "unbounded".into());
            outln!(
                "  [{i}] {}: initial {} pages, maximum {max}{}{}{}",
                module.memory_name(i as u32),
                memory.initial,
                if memory.memory64 { ", 64-bit" } else { "" },
                if memory.shared { ", shared" } else { "" },
                if memory.imported { ", imported" } else { "" }
            );
        }
    }

    if !module.globals.is_empty() {
        outln!();
        outln!("Globals ({}):", module.globals.len());
        for (i, global) in module.globals.iter().enumerate() {
            let name = module
                .global_name(i as u32)
                .unwrap_or_else(|| format!("global{i}"));
            let init = global
                .init
                .as_ref()
                .map(|v| format!(" = {v}"))
                .unwrap_or_else(|| {
                    if global.imported {
                        " (imported)".into()
                    } else {
                        String::new()
                    }
                });
            outln!(
                "  [{i}] {name}: {}{}{init}",
                if global.mutable { "mut " } else { "" },
                global.ty
            );
        }
    }

    if !module.tables.is_empty() {
        outln!();
        outln!("Tables ({}):", module.tables.len());
        for (i, table) in module.tables.iter().enumerate() {
            let max = table
                .maximum
                .map(|m| m.to_string())
                .unwrap_or_else(|| "unbounded".into());
            outln!(
                "  [{i}] {} initial {}, maximum {max}",
                table.element,
                table.initial
            );
        }
    }

    if !module.data_segments.is_empty() {
        let total: usize = module.data_segments.iter().map(|d| d.size).sum();
        outln!();
        outln!(
            "Data      : {} segments, {total} bytes",
            module.data_segments.len()
        );
    }

    if !module.custom_sections.is_empty() {
        outln!();
        outln!("Custom sections ({}):", module.custom_sections.len());
        for section in &module.custom_sections {
            outln!("  {:<24} {} bytes", section.name, section.size);
        }
    }

    if let Some(debug) = debug {
        let files = debug.lines().files_with_code();
        if !files.is_empty() {
            outln!();
            outln!("Source files ({}):", files.len());
            for file in files.iter().take(50) {
                if let Some(path) = debug.lines().file_path(*file) {
                    outln!("  {path}");
                }
            }
            if files.len() > 50 {
                outln!("  ... and {} more", files.len() - 50);
            }
        }
    }
}

fn report(module: &WasmModule, debug: Option<&DebugInfo>, path: &str, size: usize) -> Value {
    let functions: Vec<Value> = module
        .functions
        .iter()
        .map(|f| {
            json!({
                "index": f.index,
                "name": function_name(module, debug, f.index),
                "signature": module.func_sig(f.index).map(|s| s.to_string()),
                "imported": f.import.is_some(),
                "bodySize": f.body.as_ref().map(|b| b.range.end - b.range.start),
                "instructions": f.body.as_ref().map(|b| b.instructions.len()),
                "location": function_location(module, debug, f.index),
            })
        })
        .collect();
    let source_files: Vec<String> = debug
        .map(|d| {
            d.lines()
                .files_with_code()
                .into_iter()
                .filter_map(|f| d.lines().file_path(f).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "path": path,
        "size": size,
        "name": module.names.module,
        "start": module.start,
        "producers": module.producers.iter().map(|p| json!({
            "field": p.name,
            "values": p.values.iter().map(|(n, v)| json!({ "name": n, "version": v })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "debugInfo": {
            "dwarf": debug.is_some(),
            "nameSection": !module.names.functions.is_empty(),
            "compilationUnits": debug.map(|d| d.unit_count()),
            "sourceFiles": source_files,
            "producers": debug.map(|d| d.producers().to_vec()).unwrap_or_default(),
        },
        "types": module.types.len(),
        "imports": module.imports.iter().map(|i| json!({
            "module": i.module,
            "name": i.name,
            "kind": i.kind.as_str(),
            "signature": (i.kind == ExternKind::Func).then(|| module.func_sig(i.index).map(|s| s.to_string())).flatten(),
        })).collect::<Vec<_>>(),
        "exports": module.exports.iter().map(|e| json!({
            "name": e.name,
            "kind": e.kind.as_str(),
            "index": e.index,
        })).collect::<Vec<_>>(),
        "functions": functions,
        "memories": module.memories.iter().enumerate().map(|(i, m)| json!({
            "index": i,
            "name": module.memory_name(i as u32),
            "initialPages": m.initial,
            "maximumPages": m.maximum,
            "memory64": m.memory64,
            "shared": m.shared,
            "imported": m.imported,
        })).collect::<Vec<_>>(),
        "globals": module.globals.iter().enumerate().map(|(i, g)| json!({
            "index": i,
            "name": module.global_name(i as u32),
            "type": g.ty.to_string(),
            "mutable": g.mutable,
            "imported": g.imported,
            "init": g.init,
        })).collect::<Vec<_>>(),
        "tables": module.tables.iter().map(|t| json!({
            "element": t.element,
            "initial": t.initial,
            "maximum": t.maximum,
        })).collect::<Vec<_>>(),
        "dataSegments": module.data_segments.len(),
        "dataBytes": module.data_segments.iter().map(|d| d.size).sum::<usize>(),
        "customSections": module.custom_sections.iter().map(|s| json!({
            "name": s.name,
            "size": s.size,
        })).collect::<Vec<_>>(),
    })
}
