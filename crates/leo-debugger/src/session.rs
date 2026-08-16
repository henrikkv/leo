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

use crate::{
    breakpoints::{BreakpointKey, BreakpointSet},
    hook::{DebugCommand, DebugEvent, HookState, Phase, make_hook},
};

use leo_passes::ProgramDebugMap;
use snarkvm::{
    circuit::AleoTestnetV0,
    prelude::{
        Execution,
        Identifier,
        PrivateKey,
        Program,
        ProgramID,
        TestnetV0,
        Value,
        store::{FinalizeStore, helpers::memory::FinalizeMemory},
    },
    synthesizer::program::FinalizeGlobalState,
};
use snarkvm_synthesizer_process::{Process, debug_session_guard, set_debug_hook};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use indexmap::IndexMap;

use std::{
    str::FromStr,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

type CurrentNetwork = TestnetV0;
type CurrentAleo = AleoTestnetV0;

#[derive(Clone)]
pub struct ProgramSource {
    pub name: String,
    pub bytecode: String,
    pub debug_info: Option<ProgramDebugMap>,
}

pub struct SessionConfig {
    pub programs: Vec<ProgramSource>,
    pub private_key: PrivateKey<CurrentNetwork>,
    pub program_id: String,
    pub function_name: String,
    pub inputs: Vec<String>,
    pub initial_breakpoints: Vec<BreakpointKey>,
}

pub struct Session {
    pub cmd_tx: Sender<DebugCommand>,
    pub evt_rx: Receiver<DebugEvent>,
    pub state: Arc<Mutex<HookState>>,
    pub handle: JoinHandle<()>,
}

pub struct DebugContext {
    evt_tx: Sender<DebugEvent>,
    state: Arc<Mutex<HookState>>,
    cmd_rx: Mutex<Option<Receiver<DebugCommand>>>,
}

impl DebugContext {
    pub fn install_hook(&self) {
        if let Some(cmd_rx) = self.cmd_rx.lock().expect("debug context mutex poisoned").take() {
            set_debug_hook(Some(make_hook(self.state.clone(), self.evt_tx.clone(), cmd_rx)));
        }
    }

    pub fn send_event(&self, event: DebugEvent) {
        let _ = self.evt_tx.send(event);
    }

    pub fn skip_instructions(&self, suppress: bool) {
        self.state.lock().expect("debug hook state mutex poisoned").skip_instructions = suppress;
    }
}

impl Session {
    pub fn spawn(config: SessionConfig) -> Self {
        let SessionConfig { programs, private_key, program_id, function_name, inputs, initial_breakpoints } = config;
        let programs = Arc::new(programs);
        let work_programs = programs.clone();
        let (session, _result) = Self::spawn_around(&programs, initial_breakpoints, move |context| {
            run(&work_programs, &private_key, &program_id, &function_name, &inputs, context)
        });
        session
    }

    pub fn spawn_around<T: Send + 'static>(
        programs: &[ProgramSource],
        initial_breakpoints: Vec<BreakpointKey>,
        work: impl FnOnce(&DebugContext) -> Result<T> + Send + 'static,
    ) -> (Self, Arc<Mutex<Option<Result<T>>>>) {
        let mut debug_maps = IndexMap::new();
        for program in programs {
            if let Some(debug_info) = &program.debug_info {
                debug_maps.insert(program.name.clone(), debug_info.clone());
            }
        }
        let mut breakpoints = BreakpointSet::default();
        for key in initial_breakpoints {
            breakpoints.insert(key);
        }
        let state = Arc::new(Mutex::new(HookState { breakpoints, debug_maps, skip_instructions: false }));

        let (cmd_tx, cmd_rx) = unbounded();
        let (evt_tx, evt_rx) = unbounded();

        let result_slot = Arc::new(Mutex::new(None));
        let work_slot = result_slot.clone();
        let hook_state = state.clone();
        let hook_evt_tx = evt_tx.clone();

        let handle = std::thread::spawn(move || {
            let _session_guard = debug_session_guard();
            let context =
                DebugContext { evt_tx: hook_evt_tx.clone(), state: hook_state, cmd_rx: Mutex::new(Some(cmd_rx)) };
            let result = work(&context);
            set_debug_hook(None);
            match &result {
                Ok(_) => {
                    let _ = hook_evt_tx.send(DebugEvent::Finished);
                }
                Err(error) => {
                    let _ = hook_evt_tx.send(DebugEvent::Halted { reason: full_chain(error) });
                }
            }
            *work_slot.lock().expect("debug result mutex poisoned") = Some(result);
        });

        (Self { cmd_tx, evt_rx, state, handle }, result_slot)
    }
}

fn full_chain(error: &anyhow::Error) -> String {
    error.chain().map(|cause| cause.to_string()).collect::<Vec<_>>().join(": ")
}

fn run(
    programs: &[ProgramSource],
    private_key: &PrivateKey<CurrentNetwork>,
    program_id_str: &str,
    function_name_str: &str,
    inputs: &[String],
    context: &DebugContext,
) -> Result<()> {
    let process = Process::<CurrentNetwork>::load().context("failed to load a fresh snarkVM process")?;
    let mut parsed_programs = Vec::with_capacity(programs.len());
    for program in programs {
        let parsed = Program::<CurrentNetwork>::from_str(&program.bytecode)
            .with_context(|| format!("failed to parse bytecode for '{}'", program.name))?;
        process.lock().add_program(&parsed).with_context(|| format!("failed to add program '{}'", program.name))?;
        parsed_programs.push(parsed);
    }

    let finalize_store = FinalizeStore::<CurrentNetwork, FinalizeMemory<_>>::open(aleo_std::StorageMode::Production)
        .context("failed to open an in-memory finalize store")?;
    for parsed in &parsed_programs {
        for mapping_name in parsed.mappings().keys() {
            let _ = finalize_store.initialize_mapping(*parsed.id(), *mapping_name);
        }
    }

    let program_id = ProgramID::<CurrentNetwork>::from_str(program_id_str)
        .with_context(|| format!("invalid program ID '{program_id_str}'"))?;
    let function_id = Identifier::<CurrentNetwork>::from_str(function_name_str)
        .with_context(|| format!("invalid function name '{function_name_str}'"))?;
    let parsed_inputs = inputs
        .iter()
        .map(|s| Value::<CurrentNetwork>::from_str(s).with_context(|| format!("invalid input '{s}'")))
        .collect::<Result<Vec<_>>>()?;

    let rng = &mut rand::rng();

    let authorization = process
        .authorize::<CurrentAleo, _>(private_key, program_id, function_id, parsed_inputs.iter(), rng)
        .context("failed to authorize the call")?;

    context.install_hook();
    process.evaluate::<CurrentAleo>(authorization.clone()).context("evaluation failed")?;

    let has_finalize = parsed_programs
        .iter()
        .find(|p| p.id() == &program_id)
        .and_then(|p| p.get_function(&function_id).ok())
        .is_some_and(|f| f.finalize_logic().is_some());

    if has_finalize {
        let execution = Execution::from(authorization.transitions().into_values(), Default::default(), None)
            .context("failed to build an execution from the authorized transitions")?;

        context.send_event(DebugEvent::PhaseChange(Phase::Finalize));
        let state =
            FinalizeGlobalState::new_genesis::<CurrentNetwork>().context("failed to build finalize global state")?;
        process
            .lock()
            .finalize_execution(state, &finalize_store, &execution, None)
            .map_err(|error| anyhow::anyhow!("finalization failed: {error}"))?;
    }

    Ok(())
}
