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
    breakpoints::{BreakpointKey, ReverseIndex},
    hook::{Context, DebugEvent, Phase},
    session::{ProgramSource, Session, SessionConfig},
};

use leo_passes::{DebugInstructions, DebugLoc, ProgramDebugMap};
use snarkvm::prelude::{PrivateKey, TestnetV0};

use indexmap::IndexMap;
use std::time::Duration;

fn demo_program() -> (ProgramSource, ReverseIndex) {
    let bytecode = r"
program debug_info.aleo;

mapping counts:
    key as u32.public;
    value as u32.public;

function bump:
    input r0 as u32.private;
    add r0 r0 into r1;
    async bump r1 into r2;
    output r2 as debug_info.aleo/bump.future;

finalize bump:
    input r0 as u32.public;
    set r0 into counts[0u32];
"
    .to_string();

    let mut functions = IndexMap::new();
    functions.insert("bump".to_string(), DebugInstructions {
        instructions: vec![Some(DebugLoc { file: 0, line: 9, col: 5 }), None],
    });
    let mut finalizes = IndexMap::new();
    finalizes.insert("bump".to_string(), DebugInstructions {
        instructions: vec![Some(DebugLoc { file: 0, line: 14, col: 5 })],
    });

    let debug_info = ProgramDebugMap {
        source_files: vec!["main.leo".to_string()],
        functions,
        closures: IndexMap::new(),
        finalizes,
        views: IndexMap::new(),
        constructor: None,
    };

    let reverse_index = ReverseIndex::build(&debug_info);
    (ProgramSource { name: "debug_info.aleo".to_string(), bytecode, debug_info: Some(debug_info) }, reverse_index)
}

fn recv(session: &Session) -> DebugEvent {
    session.evt_rx.recv_timeout(Duration::from_secs(10)).unwrap()
}

fn recv_meaningful(session: &Session) -> DebugEvent {
    loop {
        let event = recv(session);
        if !matches!(event, DebugEvent::Progress(_)) {
            return event;
        }
    }
}

#[test]
fn test_breakpoint_hit_then_continue_reaches_finalize_and_finishes() {
    let (program, reverse_index) = demo_program();

    let (function, context, index, resolved_line) = reverse_index.resolve(0, 9).unwrap();
    assert_eq!(function, "bump");
    assert_eq!(context, Context::Transition);
    assert_eq!(index, 0);
    assert_eq!(resolved_line, 9);

    let rng = &mut rand::rng();
    let private_key = PrivateKey::<TestnetV0>::new(rng).unwrap();

    let session = Session::spawn(SessionConfig {
        programs: vec![program],
        private_key,
        program_id: "debug_info.aleo".to_string(),
        function_name: "bump".to_string(),
        inputs: vec!["5u32".to_string()],
        initial_breakpoints: vec![BreakpointKey {
            program: "debug_info.aleo".to_string(),
            function: function.clone(),
            context,
            index,
        }],
    });

    match recv(&session) {
        DebugEvent::Stopped(position) => {
            assert_eq!(position.program, "debug_info.aleo");
            assert_eq!(position.function, "bump");
            assert_eq!(position.context, Context::Transition);
            assert_eq!(position.index, 0);
            assert_eq!(position.depth, 0);
            let loc = position.loc.unwrap();
            assert_eq!((loc.file, loc.line), (0, 9));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    session.cmd_tx.send(crate::DebugCommand::Continue).unwrap();

    match recv_meaningful(&session) {
        DebugEvent::PhaseChange(Phase::Finalize) => {}
        other => panic!("unexpected event: {other:?}"),
    }
    match recv_meaningful(&session) {
        DebugEvent::Finished => {}
        other => panic!("unexpected event: {other:?}"),
    }

    session.handle.join().unwrap();
}
