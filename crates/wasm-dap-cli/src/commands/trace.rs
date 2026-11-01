use std::io::Write;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::json;
use wasm_dap_core::debugger::EntryPoint;
use wasm_dap_core::dwarf::SourceMap;
use wasm_dap_core::launch::{default_entry, parse_dir};
use wasm_dap_core::runtime::{Program, WasiOptions};
use wasm_dap_core::trace::{ExitStatus, TraceEvent, TraceOptions, TraceValue, trace};

use crate::cli::TraceArgs;

pub fn run(args: TraceArgs) -> Result<ExitCode> {
    let program = Program::load(&args.file)
        .with_context(|| format!("failed to load {}", args.file.display()))?;
    let entry = match &args.invoke {
        Some(name) => EntryPoint::Function {
            name: name.clone(),
            args: args.args.iter().map(|a| json!(a)).collect(),
        },
        None => default_entry(&program)?,
    };
    let program_args = if matches!(entry, EntryPoint::Function { .. }) {
        Vec::new()
    } else {
        args.args.clone()
    };
    let cwd = std::env::current_dir()?;
    let dirs = args
        .dirs
        .iter()
        .map(|spec| parse_dir(&cwd, spec))
        .collect::<wasm_dap_core::Result<Vec<_>>>()?;
    let env = args
        .env
        .iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => Ok((k.to_string(), v.to_string())),
            None => bail!("invalid environment variable `{kv}`; use KEY=VALUE"),
        })
        .collect::<Result<Vec<_>>>()?;
    let source_map = args
        .source_map
        .iter()
        .map(|m| match m.split_once('=') {
            Some((from, to)) => Ok((from.to_string(), to.to_string())),
            None => bail!("invalid source map `{m}`; use FROM=TO"),
        })
        .collect::<Result<Vec<_>>>()?;
    let argv0 = args
        .file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut wasi_args = vec![argv0];
    wasi_args.extend(program_args);

    let options = TraceOptions {
        entry,
        wasi: WasiOptions {
            args: wasi_args,
            env,
            dirs,
            ..WasiOptions::default()
        },
        max_events: args.max_events,
        instructions: args.instructions,
        capture_output: args.json,
        source_map: SourceMap::new(source_map),
    };

    let collected: Arc<Mutex<Vec<TraceEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let summary = if args.json {
        let sink = collected.clone();
        trace(program, options, move |event| {
            if let Ok(mut events) = sink.lock() {
                events.push(event.clone());
            }
        })?
    } else {
        trace(program, options, print_event)?
    };

    if args.json {
        let events = collected.lock().map(|e| e.clone()).unwrap_or_default();
        let report = json!({ "events": events, "summary": summary });
        outln!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let exit = match &summary.exit {
            ExitStatus::Returned { results } if results.is_empty() => "returned".to_string(),
            ExitStatus::Returned { results } => {
                format!("returned {}", format_values(results, false))
            }
            ExitStatus::Exited { code } => format!("exited with code {code}"),
            ExitStatus::Trapped { message } => format!("trapped: {message}"),
        };
        eprintln!(
            "\n{exit} | {} calls, max depth {}, {} events{}, {} ms",
            summary.calls,
            summary.max_depth,
            summary.events,
            if summary.truncated {
                " (limit reached)"
            } else {
                ""
            },
            summary.elapsed_ms
        );
    }

    Ok(match summary.exit {
        ExitStatus::Returned { .. } => ExitCode::SUCCESS,
        ExitStatus::Exited { code } => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        ExitStatus::Trapped { .. } => ExitCode::FAILURE,
    })
}

fn format_values(values: &[TraceValue], named: bool) -> String {
    let parts: Vec<String> = values
        .iter()
        .map(|v| match (&v.name, named) {
            (Some(name), true) => format!("{name}={}", v.value),
            _ => v.value.clone(),
        })
        .collect();
    parts.join(", ")
}

fn print_event(event: &TraceEvent) {
    let mut out = std::io::stdout().lock();
    let indent = |depth: usize| "  ".repeat(depth.saturating_sub(1));
    let written = match event {
        TraceEvent::Call {
            depth,
            function,
            args,
            location,
            ..
        } => {
            let location = location
                .as_ref()
                .map(|l| format!("  [{l}]"))
                .unwrap_or_default();
            writeln!(
                out,
                "{}→ {function}({}){location}",
                indent(*depth),
                format_values(args, true)
            )
        }
        TraceEvent::Return {
            depth,
            function,
            results,
            ..
        } => match results {
            Some(results) if !results.is_empty() => {
                writeln!(
                    out,
                    "{}← {function} = {}",
                    indent(*depth),
                    format_values(results, false)
                )
            }
            Some(_) => writeln!(out, "{}← {function}", indent(*depth)),
            None => writeln!(out, "{}← {function} (unwound)", indent(*depth)),
        },
        TraceEvent::Instruction {
            depth, pc, text, ..
        } => writeln!(out, "{}  {pc}  {text}", indent(*depth)),
        TraceEvent::Output { text, .. } => write!(out, "{text}"),
        TraceEvent::Trap { message, backtrace } => {
            writeln!(out, "trap: {message}").and_then(|()| {
                backtrace
                    .iter()
                    .enumerate()
                    .try_for_each(|(i, frame)| writeln!(out, "  {i}: {frame}"))
            })
        }
    };
    if let Err(error) = written.and_then(|()| out.flush()) {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("error: failed to write the trace: {error}");
        std::process::exit(1);
    }
}
