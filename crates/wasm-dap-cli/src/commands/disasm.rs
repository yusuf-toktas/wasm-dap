use std::process::ExitCode;

use anyhow::{Result, bail};

use super::load_static;
use crate::cli::DisasmArgs;

pub fn run(args: DisasmArgs) -> Result<ExitCode> {
    let (module, debug) = load_static(&args.file)?;
    let disassembly = module.disassembly()?;

    let selected: Vec<u32> = match &args.function {
        Some(query) => {
            let mut indices = module.find_functions(query);
            if let Some(debug) = &debug {
                for subprogram in debug.find_subprograms(query) {
                    if let Some(f) = module.function_at(subprogram.low) {
                        indices.push(f.index);
                    }
                }
            }
            indices.sort_unstable();
            indices.dedup();
            indices.retain(|i| module.function(*i).is_some_and(|f| f.body.is_some()));
            if indices.is_empty() {
                bail!("no defined function matches `{query}`");
            }
            indices
        }
        None => module.defined_functions().map(|f| f.index).collect(),
    };

    for (n, index) in selected.iter().enumerate() {
        let function = module.function(*index).expect("selected function exists");
        let body = function
            .body
            .as_ref()
            .expect("selected function has a body");
        let name = debug
            .as_ref()
            .and_then(|d| d.subprogram_at(body.range.start))
            .map(|s| s.name.clone())
            .unwrap_or_else(|| module.display_function_name(*index));
        let sig = module
            .func_sig(*index)
            .map(|s| s.to_string())
            .unwrap_or_default();
        if n > 0 {
            outln!();
        }
        outln!("func[{index}] {name} {sig}");
        let mut last_line: Option<(u32, u32)> = None;
        for instruction in &body.instructions {
            let text = disassembly
                .instruction_text(instruction.offset)
                .unwrap_or("?");
            let annotation = if args.no_source {
                None
            } else {
                debug.as_ref().and_then(|d| {
                    let line = d.lines().location(instruction.offset)?;
                    let key = (line.file, line.line);
                    if last_line == Some(key) {
                        return None;
                    }
                    last_line = Some(key);
                    let path = d.lines().file_path(line.file)?;
                    Some(format!("{path}:{}", line.line))
                })
            };
            match annotation {
                Some(location) => {
                    outln!("  {:#010x}  {text:<40} ;; {location}", instruction.offset)
                }
                None => outln!("  {:#010x}  {text}", instruction.offset),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
