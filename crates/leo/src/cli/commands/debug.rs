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

use super::*;

use leo_ast::{NetworkName, TEST_PRIVATE_KEY};
use leo_debugger::{
    breakpoints::{BreakpointKey, ReverseIndex, resolve_file_index},
    session::{ProgramSource, Session, SessionConfig},
};
use leo_package::{Package, ProgramData};

use clap::Parser;
use snarkvm::prelude::{PrivateKey, Program, ProgramID, TestnetV0};

use std::str::FromStr;

#[derive(Parser, Debug)]
pub struct LeoDebug {
    #[clap(
        name = "NAME",
        help = "The name of the function to debug, e.g `helloworld.aleo::main` or `main`.",
        default_value = "main"
    )]
    pub(crate) name: String,
    #[clap(name = "INPUTS", help = "The program inputs e.g. `1u32`, `record1...`, or `{ owner: ...}`")]
    pub(crate) inputs: Vec<String>,
    #[clap(flatten)]
    pub(crate) env_override: EnvOptions,
    #[clap(flatten)]
    pub(crate) key_override: PrivateKeyOptions,
    #[clap(flatten)]
    pub(crate) build_options: BuildOptions,
    #[clap(long = "with", value_delimiter = ',')]
    pub(crate) with: Vec<String>,
    #[clap(
        long = "break",
        help = "Set breakpoints at a list of 'file:line' or 'program.aleo:file:line'",
        value_delimiter = ','
    )]
    pub(crate) breakpoints: Vec<String>,
}

impl Command for LeoDebug {
    type Input = Package;
    type Output = ();

    fn log_span(&self) -> Span {
        tracing::span!(tracing::Level::INFO, "Leo")
    }

    fn prelude(&self, context: Context) -> Result<Self::Input> {
        let mut options = self.build_options.clone();
        options.debug_info = true;
        LeoBuild { env_override: self.env_override.clone(), options, rename: None }.execute(context)
    }

    fn apply(self, context: Context, package: Package) -> Result<()> {
        if package.compilation_units.last().is_some_and(|p| p.kind.is_library()) {
            return Err(crate::errors::custom("`leo debug` does not support library packages.").into());
        }

        match get_network(&self.env_override.network) {
            Ok(NetworkName::TestnetV0) | Err(_) => {}
            Ok(_) => return Err(crate::errors::custom("`leo debug` only supports TestnetV0.").into()),
        }

        handle_debug(self, context, package)
    }
}

type N = TestnetV0;

fn handle_debug(command: LeoDebug, context: Context, package: Package) -> Result<()> {
    let private_key = match get_private_key::<N>(&command.key_override.private_key) {
        Ok(private_key) => private_key,
        Err(_) => {
            println!("⚠️ No valid private key specified, defaulting to '{TEST_PRIVATE_KEY}'.");
            PrivateKey::<N>::from_str(TEST_PRIVATE_KEY).expect("Failed to parse the test private key")
        }
    };

    let (program_name, function_name) = match command.name.split_once('/').or_else(|| command.name.split_once("::")) {
        Some((program_name, function_name)) => (with_aleo_suffix(program_name), function_name.to_string()),
        None => (
            with_aleo_suffix(
                &package
                    .compilation_units
                    .last()
                    .expect("There must be at least one program in a Leo package")
                    .name
                    .to_string(),
            ),
            command.name,
        ),
    };

    let mut programs = Vec::new();
    for unit in package.compilation_units.iter().filter(|unit| !unit.kind.is_library()) {
        let unit_name = unit.name.to_string();
        let bytecode = match &unit.data {
            ProgramData::Bytecode(bytecode) => bytecode.to_string(),
            ProgramData::SourcePath { .. } => {
                let bytecode_path = package.unit_bytecode_path(&unit_name);
                std::fs::read_to_string(&bytecode_path).map_err(|e| {
                    crate::errors::custom(format!("Failed to read bytecode at {}: {e}", bytecode_path.display()))
                })?
            }
        };
        let debug_info = leo_debugger::debug_info::load(&package.unit_debug_path(&unit_name))
            .map_err(|e| crate::errors::custom(format!("Failed to load debug info for '{unit_name}': {e}")))?;
        programs.push(ProgramSource { name: with_aleo_suffix(&unit_name), bytecode, debug_info });
    }

    for entry in &command.with {
        let path = std::path::Path::new(entry);
        if path.is_file() {
            println!("📂 Loading local program from {entry}...");
            let bytecode = std::fs::read_to_string(path)
                .map_err(|e| crate::errors::custom(format!("Failed to read program file '{entry}': {e}")))?;
            let program = Program::<N>::from_str(&bytecode)
                .map_err(|e| crate::errors::custom(format!("Failed to parse program from '{entry}': {e}")))?;
            programs.push(ProgramSource { name: program.id().to_string(), bytecode, debug_info: None });
        } else if path.exists() {
            return Err(crate::errors::custom(format!("'{entry}' exists but is not a file.")).into());
        } else {
            let endpoint = get_endpoint(&command.env_override.endpoint)?;
            let name = if entry.ends_with(".aleo") { entry.clone() } else { format!("{entry}.aleo") };
            println!("⬇️  Fetching remote program {name} and its dependencies from {endpoint}...");
            let program_id = ProgramID::<N>::from_str(&name)
                .map_err(|e| crate::errors::custom(format!("Failed to parse program ID '{name}': {e}")))?;
            let fetched = load_latest_programs_from_network(
                &context,
                program_id,
                NetworkName::TestnetV0,
                &endpoint,
                command.env_override.network_retries,
            )?;
            for (program, _edition) in fetched {
                let bytecode = program.to_string();
                programs.push(ProgramSource { name: program.id().to_string(), bytecode, debug_info: None });
            }
        }
    }

    let mut initial_breakpoints =
        command.breakpoints.iter().map(|spec| resolve_breakpoint_spec(spec, &programs)).collect::<Result<Vec<_>>>()?;

    initial_breakpoints.push(BreakpointKey {
        program: program_name.clone(),
        function: function_name.clone(),
        context: leo_debugger::Context::Transition,
        index: 0,
    });

    let programs_for_tui = programs.clone();

    let session = Session::spawn(SessionConfig {
        programs,
        private_key,
        program_id: program_name,
        function_name,
        inputs: command.inputs,
        initial_breakpoints,
    });

    leo_debugger::tui::run(session, programs_for_tui)
        .map_err(|e| crate::errors::custom(format!("Debugger TUI failed: {e}")))?;

    Ok(())
}

fn with_aleo_suffix(name: &str) -> String {
    if name.ends_with(".aleo") { name.to_string() } else { format!("{name}.aleo") }
}

fn resolve_breakpoint_spec(spec: &str, programs: &[ProgramSource]) -> Result<BreakpointKey> {
    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    let (program_filter, file, line) = match parts.as_slice() {
        [file, line] => (None, *file, *line),
        [program, file, line] => (Some(*program), *file, *line),
        _ => {
            return Err(crate::errors::custom(format!(
                "Invalid breakpoint '{spec}': expected 'file:line' or 'program.aleo:file:line'"
            ))
            .into());
        }
    };
    let line: u32 =
        line.parse().map_err(|_| crate::errors::custom(format!("Invalid line number in breakpoint '{spec}'")))?;

    for program in programs {
        if program_filter.is_some_and(|filter| filter != program.name) {
            continue;
        }
        let Some(debug_info) = &program.debug_info else { continue };
        let Some(file_index) = resolve_file_index(debug_info, file) else { continue };
        let index = ReverseIndex::build(debug_info);
        if let Some((function, context, instruction_index, _resolved_line)) = index.resolve(file_index, line) {
            return Ok(BreakpointKey { program: program.name.clone(), function, context, index: instruction_index });
        }
    }
    Err(crate::errors::custom(format!("Could not resolve breakpoint '{spec}' against any loaded program's debug info"))
        .into())
}
