// Copyright 2025 Meta-Hybrid Mount Authors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::OnceLock;

use anyhow::Result;
// [FIX] Removed unused NukeExt4Sysfs
use ksu::TryUmount;

pub static TMPFS: OnceLock<String> = OnceLock::new();

pub fn send_unmountable(target: &str) -> Result<()> {
    TryUmount::new()
        .target(target)
        .run()
        .map_err(|e| anyhow::anyhow!(e))
}

pub fn commit() -> Result<()> {
    if let Some(tmpfs) = TMPFS.get() {
        TryUmount::new()
            .target(tmpfs)
            .run()
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    Ok(())
}
