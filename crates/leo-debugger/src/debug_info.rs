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

use leo_passes::ProgramDebugMap;

use anyhow::{Context, Result};

use std::path::Path;

pub fn load(path: &Path) -> Result<Option<ProgramDebugMap>> {
    if !path.exists() {
        return Ok(None);
    }
    let json =
        std::fs::read_to_string(path).with_context(|| format!("failed to read debug info at {}", path.display()))?;
    let debug_map =
        serde_json::from_str(&json).with_context(|| format!("failed to parse debug info at {}", path.display()))?;
    Ok(Some(debug_map))
}
