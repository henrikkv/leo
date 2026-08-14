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

use crate::hook::Context;

use leo_passes::ProgramDebugMap;

use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BreakpointKey {
    pub program: String,
    pub function: String,
    pub context: Context,
    pub index: usize,
}

#[derive(Default)]
pub struct BreakpointSet {
    keys: HashSet<BreakpointKey>,
}

impl BreakpointSet {
    pub fn insert(&mut self, key: BreakpointKey) -> bool {
        self.keys.insert(key)
    }

    pub fn remove(&mut self, key: &BreakpointKey) -> bool {
        self.keys.remove(key)
    }

    pub fn contains(&self, program: &str, function: &str, context: Context, index: usize) -> bool {
        self.keys
            .iter()
            .any(|k| k.index == index && k.context == context && k.function == function && k.program == program)
    }

    pub fn iter(&self) -> impl Iterator<Item = &BreakpointKey> {
        self.keys.iter()
    }
}

pub struct ReverseIndex {
    by_file_function_context: Vec<((usize, String, Context), Vec<(u32, usize)>)>,
}

impl ReverseIndex {
    pub fn build(debug_map: &ProgramDebugMap) -> Self {
        let mut grouped: Vec<((usize, String, Context), Vec<(u32, usize)>)> = Vec::new();

        let sections: [(_, Context); 4] = [
            (&debug_map.functions, Context::Transition),
            (&debug_map.closures, Context::Transition),
            (&debug_map.views, Context::Transition),
            (&debug_map.finalizes, Context::Finalize),
        ];
        for (map, context) in sections {
            for (name, instructions) in map {
                for (index, loc) in instructions.instructions.iter().enumerate() {
                    let Some(loc) = loc else { continue };
                    let key = (loc.file, name.clone(), context);
                    match grouped.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, entries)) => entries.push((loc.line, index)),
                        None => grouped.push((key, vec![(loc.line, index)])),
                    }
                }
            }
        }
        if let Some(constructor) = &debug_map.constructor {
            for (index, loc) in constructor.instructions.iter().enumerate() {
                let Some(loc) = loc else { continue };
                let key = (loc.file, "constructor".to_string(), Context::Finalize);
                match grouped.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, entries)) => entries.push((loc.line, index)),
                    None => grouped.push((key, vec![(loc.line, index)])),
                }
            }
        }

        for (_, entries) in &mut grouped {
            entries.sort_unstable();
        }
        Self { by_file_function_context: grouped }
    }

    pub fn resolve(&self, file: usize, line: u32) -> Option<(String, Context, usize, u32)> {
        let mut best: Option<(u32, &str, Context, usize)> = None;
        for ((f, function, context), entries) in &self.by_file_function_context {
            if *f != file {
                continue;
            }
            let position = entries.partition_point(|(l, _)| *l < line);
            let Some(&(found_line, index)) = entries.get(position) else { continue };
            if best.is_none_or(|(best_line, _, _, _)| found_line < best_line) {
                best = Some((found_line, function.as_str(), *context, index));
            }
        }
        best.map(|(found_line, function, context, index)| (function.to_string(), context, index, found_line))
    }
}

pub fn resolve_file_index(debug_map: &ProgramDebugMap, filename: &str) -> Option<usize> {
    debug_map.source_files.iter().position(|f| {
        f == filename || std::path::Path::new(f).file_name().is_some_and(|name| name.to_string_lossy() == filename)
    })
}
