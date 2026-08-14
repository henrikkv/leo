// Copyright (C) 2019-2026 Provable Inc.
// This file is part of the Leo library.

// The Leo library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The Leo library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the Leo library. If not, see <https://www.gnu.org/licenses/>.

use crate::breakpoints::BreakpointSet;

use leo_passes::ProgramDebugMap;
use snarkvm_synthesizer_process::{DebugAction, DebugPoint, DebugPointKind, Frame};

use crossbeam_channel::{Receiver, Sender};
use indexmap::IndexMap;

use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLoc {
    pub file: usize,
    pub line: u32,
    pub col: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Context {
    Transition,
    Finalize,
}

impl From<DebugPointKind> for Context {
    fn from(kind: DebugPointKind) -> Self {
        match kind {
            DebugPointKind::Instruction => Self::Transition,
            DebugPointKind::FinalizeCommand => Self::Finalize,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Transition,
    Finalize,
}

#[derive(Clone, Debug)]
pub struct Position {
    pub program: String,
    pub function: String,
    pub context: Context,
    pub index: usize,
    pub text: String,
    pub depth: usize,
    pub frames: Vec<Frame>,
    pub loc: Option<SourceLoc>,
    pub registers: Vec<(u64, String)>,
}

#[derive(Clone, Debug)]
pub enum DebugEvent {
    Stopped(Position),
    Progress(Position),
    PhaseChange(Phase),
    Finished,
    Halted { reason: String },
}

#[derive(Clone, Debug)]
pub enum DebugCommand {
    Continue,
    StepInto,
    StepOverLine,
    StepInstr,
    NextInstr,
    Quit,
}

#[derive(Default)]
pub struct HookState {
    pub breakpoints: BreakpointSet,
    pub debug_maps: IndexMap<String, ProgramDebugMap>,
}

impl HookState {
    fn source_loc(&self, program: &str, kind: DebugPointKind, function: &str, index: usize) -> Option<SourceLoc> {
        let map = self.debug_maps.get(program)?;
        let instructions = match kind {
            DebugPointKind::FinalizeCommand => map.finalizes.get(function),
            DebugPointKind::Instruction => {
                map.functions.get(function).or_else(|| map.closures.get(function)).or_else(|| map.views.get(function))
            }
        }?;
        let loc = instructions.instructions.get(index)?.as_ref()?;
        Some(SourceLoc { file: loc.file, line: loc.line, col: loc.col })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Granularity {
    Line,
    Instruction,
}

struct PendingStep {
    depth_at_issue: usize,
    over: bool,
    granularity: Granularity,
    start_loc: Option<(usize, u32)>,
}

impl PendingStep {
    fn should_pause(&self, current_loc: Option<SourceLoc>, depth: usize) -> bool {
        if self.over && depth > self.depth_at_issue {
            return false;
        }
        match self.granularity {
            Granularity::Instruction => true,
            Granularity::Line => match (self.start_loc, current_loc) {
                (Some(start), Some(cur)) => (cur.file, cur.line) != start,
                _ => true,
            },
        }
    }
}

pub fn make_hook(
    state: Arc<Mutex<HookState>>,
    evt_tx: Sender<DebugEvent>,
    cmd_rx: Receiver<DebugCommand>,
) -> Box<dyn FnMut(DebugPoint) -> DebugAction> {
    let mut pending: Option<PendingStep> = None;

    Box::new(move |point: DebugPoint| {
        let guard = state.lock().expect("debug hook state mutex poisoned");
        let loc = guard.source_loc(&point.program_id, point.kind, &point.function_name, point.index);
        let should_break =
            guard.breakpoints.contains(&point.program_id, &point.function_name, point.kind.into(), point.index);
        drop(guard);

        let position = Position {
            program: point.program_id.clone(),
            function: point.function_name.clone(),
            context: point.kind.into(),
            index: point.index,
            text: point.text.clone(),
            depth: point.depth,
            frames: snarkvm_synthesizer_process::current_frames(),
            loc,
            registers: point.registers.clone(),
        };

        let should_pause = should_break || pending.as_ref().is_some_and(|step| step.should_pause(loc, point.depth));
        if !should_pause {
            let _ = evt_tx.send(DebugEvent::Progress(position));
            return DebugAction::Continue;
        }

        loop {
            if evt_tx.send(DebugEvent::Stopped(position.clone())).is_err() {
                return DebugAction::Halt("debug event channel closed".to_string());
            }
            let command = match cmd_rx.recv() {
                Ok(command) => command,
                Err(_) => return DebugAction::Halt("debug command channel closed".to_string()),
            };
            match command {
                DebugCommand::Continue => {
                    pending = None;
                    return DebugAction::Continue;
                }
                DebugCommand::Quit => return DebugAction::Halt("quit".to_string()),
                DebugCommand::StepInto => {
                    pending = Some(PendingStep {
                        depth_at_issue: point.depth,
                        over: false,
                        granularity: Granularity::Line,
                        start_loc: loc.map(|l| (l.file, l.line)),
                    });
                    return DebugAction::Continue;
                }
                DebugCommand::StepOverLine => {
                    pending = Some(PendingStep {
                        depth_at_issue: point.depth,
                        over: true,
                        granularity: Granularity::Line,
                        start_loc: loc.map(|l| (l.file, l.line)),
                    });
                    return DebugAction::Continue;
                }
                DebugCommand::StepInstr => {
                    pending = Some(PendingStep {
                        depth_at_issue: point.depth,
                        over: false,
                        granularity: Granularity::Instruction,
                        start_loc: None,
                    });
                    return DebugAction::Continue;
                }
                DebugCommand::NextInstr => {
                    pending = Some(PendingStep {
                        depth_at_issue: point.depth,
                        over: true,
                        granularity: Granularity::Instruction,
                        start_loc: None,
                    });
                    return DebugAction::Continue;
                }
            }
        }
    })
}
