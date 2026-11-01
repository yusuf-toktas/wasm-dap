//! Step planning.
//!
//! Line steps single-step instructions until execution reaches the start of a
//! different source statement. Calls that should not be entered are skipped
//! by running to a temporary breakpoint at the caller's return address, which
//! avoids single-stepping through the callee.

use super::program::ProgramInfo;
use super::{Granularity, StepKind};
use crate::dwarf::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    SingleStep,
    ReturnTo { pc: u32, depth: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepPlan {
    pub kind: StepKind,
    pub granularity: Granularity,
    pub start_depth: usize,
    pub start_pc: u32,
    pub start_line: Option<(FileId, u32)>,
    /// File of the function's own code; rows from other files at the same
    /// depth come from inlined callees.
    pub home_file: Option<FileId>,
    pub phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The step is complete.
    Stop,
    /// Keep going in the current phase.
    Continue,
    /// Stop single-stepping and run to a return address.
    RunTo { pc: u32, depth: usize },
    /// Resume single-stepping after reaching a return address.
    SingleStepAgain,
}

fn line_at(program: &ProgramInfo, pc: u32) -> Option<(FileId, u32)> {
    let line = program.debug.as_ref()?.lines().location(pc)?;
    Some((line.file, line.line))
}

/// Line of the statement row covering `pc`; compiler-generated rows have none.
///
/// The covering row is used rather than an exact row start because branches,
/// such as loop back-edges, can enter a line in the middle of its row.
fn statement_line(program: &ProgramInfo, pc: u32) -> Option<(FileId, u32)> {
    let row = program.debug.as_ref()?.lines().row_at(pc)?;
    (row.is_stmt && row.line != 0).then_some((row.file, row.line))
}

fn home_file(program: &ProgramInfo, pc: u32) -> Option<FileId> {
    let first = program
        .module
        .function_at(pc)?
        .body
        .as_ref()?
        .first_instruction()?;
    line_at(program, first).map(|(file, _)| file)
}

/// Return address in the frame at `index` (0 = innermost) of `pcs`.
fn return_address(program: &ProgramInfo, pcs: &[u32], index: usize) -> Option<u32> {
    let call = *pcs.get(index)?;
    program.module.next_instruction(call)
}

impl StepPlan {
    /// Creates a plan from the paused location. `pcs` lists frame PCs, innermost first.
    pub fn new(
        program: &ProgramInfo,
        kind: StepKind,
        granularity: Granularity,
        pcs: &[u32],
    ) -> Option<Self> {
        let start_pc = *pcs.first()?;
        let start_line = line_at(program, start_pc);
        let granularity = if start_line.is_none() {
            Granularity::Instruction
        } else {
            granularity
        };
        let depth = pcs.len();
        let phase = match kind {
            StepKind::Out => {
                let pc = return_address(program, pcs, 1)?;
                Phase::ReturnTo {
                    pc,
                    depth: depth - 1,
                }
            }
            _ => Phase::SingleStep,
        };
        Some(Self {
            kind,
            granularity,
            start_depth: depth,
            start_pc,
            start_line,
            home_file: home_file(program, start_pc),
            phase,
        })
    }

    /// Decides what to do at a debug event at `pcs[0]` with depth `pcs.len()`.
    pub fn decide(&mut self, program: &ProgramInfo, pcs: &[u32]) -> Decision {
        let Some(&pc) = pcs.first() else {
            return Decision::Continue;
        };
        let depth = pcs.len();

        if let Phase::ReturnTo {
            pc: target,
            depth: target_depth,
        } = self.phase
        {
            if pc != target || depth != target_depth {
                return Decision::Continue;
            }
            if self.kind == StepKind::Out || self.granularity == Granularity::Instruction {
                return Decision::Stop;
            }
            self.phase = Phase::SingleStep;
            return Decision::SingleStepAgain;
        }

        if self.granularity == Granularity::Instruction {
            if self.kind == StepKind::Over && depth > self.start_depth {
                return self.run_to_original_frame(program, pcs);
            }
            return Decision::Stop;
        }

        if depth > self.start_depth {
            if self.kind == StepKind::Over {
                return self.run_to_original_frame(program, pcs);
            }
            if !program.has_line_info(pc) {
                return match return_address(program, pcs, 1) {
                    Some(pc) => Decision::RunTo {
                        pc,
                        depth: depth - 1,
                    },
                    None => Decision::Continue,
                };
            }
            return if statement_line(program, pc).is_some() {
                Decision::Stop
            } else {
                Decision::Continue
            };
        }

        let Some(line) = statement_line(program, pc) else {
            return Decision::Continue;
        };
        if depth < self.start_depth {
            return Decision::Stop;
        }
        if self.kind == StepKind::Over
            && let (Some(home), Some((start_file, _))) = (self.home_file, self.start_line)
            && start_file == home
            && line.0 != home
        {
            return Decision::Continue;
        }
        if Some(line) != self.start_line || pc <= self.start_pc {
            return Decision::Stop;
        }
        Decision::Continue
    }

    fn run_to_original_frame(&self, program: &ProgramInfo, pcs: &[u32]) -> Decision {
        let index = pcs.len() - self.start_depth;
        match return_address(program, pcs, index) {
            Some(pc) => Decision::RunTo {
                pc,
                depth: self.start_depth,
            },
            None => Decision::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::dwarf::SourceMap;
    use crate::module::WasmModule;

    fn program() -> ProgramInfo {
        let wasm = wat::parse_str(
            r#"(module
                (func $leaf (result i32) i32.const 1)
                (func $caller (result i32)
                    call $leaf
                    i32.const 2
                    i32.add))"#,
        )
        .unwrap();
        let module = Arc::new(WasmModule::parse(wasm).unwrap());
        ProgramInfo::new(Path::new("t.wasm"), module, None, SourceMap::default())
    }

    fn caller_instructions(program: &ProgramInfo) -> Vec<u32> {
        program
            .module
            .function(1)
            .unwrap()
            .body
            .as_ref()
            .unwrap()
            .instructions
            .iter()
            .map(|i| i.offset)
            .collect()
    }

    #[test]
    fn instruction_step_in_stops_at_next_event() {
        let program = program();
        let caller = caller_instructions(&program);
        let mut plan =
            StepPlan::new(&program, StepKind::In, Granularity::Line, &[caller[0]]).unwrap();
        assert_eq!(
            plan.granularity,
            Granularity::Instruction,
            "no DWARF falls back"
        );
        let leaf = program
            .module
            .function(0)
            .unwrap()
            .body
            .as_ref()
            .unwrap()
            .instructions[0]
            .offset;
        assert_eq!(plan.decide(&program, &[leaf, caller[0]]), Decision::Stop);
    }

    #[test]
    fn instruction_step_over_runs_to_return_address() {
        let program = program();
        let caller = caller_instructions(&program);
        let leaf = program
            .module
            .function(0)
            .unwrap()
            .body
            .as_ref()
            .unwrap()
            .instructions[0]
            .offset;
        let mut plan = StepPlan::new(
            &program,
            StepKind::Over,
            Granularity::Instruction,
            &[caller[0]],
        )
        .unwrap();
        let decision = plan.decide(&program, &[leaf, caller[0]]);
        assert_eq!(
            decision,
            Decision::RunTo {
                pc: caller[1],
                depth: 1
            }
        );
        plan.phase = Phase::ReturnTo {
            pc: caller[1],
            depth: 1,
        };
        assert_eq!(plan.decide(&program, &[caller[1]]), Decision::Stop);
    }

    #[test]
    fn step_out_targets_caller_return_address() {
        let program = program();
        let caller = caller_instructions(&program);
        let leaf = program
            .module
            .function(0)
            .unwrap()
            .body
            .as_ref()
            .unwrap()
            .instructions[0]
            .offset;
        let plan = StepPlan::new(
            &program,
            StepKind::Out,
            Granularity::Line,
            &[leaf, caller[0]],
        )
        .unwrap();
        assert_eq!(
            plan.phase,
            Phase::ReturnTo {
                pc: caller[1],
                depth: 1
            }
        );
        assert!(StepPlan::new(&program, StepKind::Out, Granularity::Line, &[caller[0]]).is_none());
    }
}
