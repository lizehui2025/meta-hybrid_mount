// Copyright 2026 Hybrid Mount Developers
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::Result;
use walkdir::WalkDir;

use crate::{
    conf::config,
    core::planner::MountPlan,
    defs,
    mount::{magic_mount, overlayfs},
    utils,
};

pub struct ExecutionResult {
    pub overlay_module_ids: Vec<String>,
    pub magic_module_ids: Vec<String>,
}

pub enum DiagnosticLevel {
    #[allow(dead_code)]
    Info,
    Warning,
    Critical,
}

pub struct DiagnosticIssue {
    pub level: DiagnosticLevel,
    pub context: String,
    pub message: String,
}

pub fn diagnose_plan(plan: &MountPlan) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();

    for op in &plan.overlay_ops {
        let target = Path::new(&op.target);

        if !target.exists() {
            issues.push(DiagnosticIssue {
                level: DiagnosticLevel::Critical,
                context: op.partition_name.clone(),
                message: format!("Target mount point does not exist: {}", op.target),
            });
        }
    }

    let all_layers: Vec<(String, &PathBuf)> = plan
        .overlay_ops
        .iter()
        .flat_map(|op| {
            op.lowerdirs.iter().map(move |path| {
                let mod_id = utils::extract_module_id(path).unwrap_or_else(|| "unknown".into());

                (mod_id, path)
            })
        })
        .collect();

    for (mod_id, layer_path) in all_layers {
        if !layer_path.exists() {
            continue;
        }

        for entry in WalkDir::new(layer_path).into_iter().flatten() {
            if entry.path_is_symlink()
                && let Ok(target) = std::fs::read_link(entry.path())
                && target.is_absolute()
                && !target.exists()
            {
                issues.push(DiagnosticIssue {
                    level: DiagnosticLevel::Warning,
                    context: mod_id.clone(),
                    message: format!(
                        "Dead absolute symlink: {} -> {}",
                        entry.path().display(),
                        target.display()
                    ),
                });
            }
        }
    }

    issues
}

pub fn execute(plan: &MountPlan, config: &config::Config) -> Result<ExecutionResult> {
    let mut final_magic_ids: HashSet<String> = plan.magic_module_ids.iter().cloned().collect();
    let mut final_overlay_ids: HashSet<String> = HashSet::new();

    log::info!(">> Phase 1: OverlayFS Execution...");

    for op in &plan.overlay_ops {
        let involved_modules: Vec<String> = op
            .lowerdirs
            .iter()
            .filter_map(|p| utils::extract_module_id(p))
            .collect();

        let lowerdir_strings: Vec<String> = op
            .lowerdirs
            .iter()
            .map(|p| p.display().to_string())
            .collect();

        let rw_root = Path::new(defs::SYSTEM_RW_DIR);
        let part_rw = rw_root.join(&op.partition_name);
        let upper = part_rw.join("upperdir");
        let work = part_rw.join("workdir");

        let (upper_opt, work_opt) = if upper.exists() && work.exists() {
            (Some(upper), Some(work))
        } else {
            (None, None)
        };

        log::info!(
            "Mounting {} [OVERLAY] (Layers: {})",
            op.target,
            lowerdir_strings.len()
        );

        match overlayfs::overlayfs::mount_overlay(
            &op.target,
            &lowerdir_strings,
            work_opt,
            upper_opt,
            &config.mountsource,
        ) {
            Ok(_) => {
                for id in involved_modules {
                    final_overlay_ids.insert(id);
                }

                #[cfg(any(target_os = "linux", target_os = "android"))]
                if !config.disable_umount {
                    if let Err(e) = crate::try_umount::send_unmountable(&op.target) {
                        log::warn!("Failed to schedule unmount for {}: {}", op.target, e);
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "OverlayFS failed for {}: {}. Fallback to Magic Mount.",
                    op.target,
                    e
                );
                for id in involved_modules {
                    final_magic_ids.insert(id);
                }
            }
        }
    }

    final_overlay_ids.retain(|id| !final_magic_ids.contains(id));

    let mut magic_queue: Vec<String> = final_magic_ids.iter().cloned().collect();
    magic_queue.sort();

    if !magic_queue.is_empty() {
        let tempdir = PathBuf::from(&config.hybrid_mnt_dir).join("magic_workspace");
        let _ = crate::try_umount::TMPFS.set(tempdir.to_string_lossy().to_string());

        log::info!(
            ">> Phase 2: Magic Mount (Fallback/Native) using {}",
            tempdir.display()
        );

        if !tempdir.exists() {
            std::fs::create_dir_all(&tempdir)?;
        }

        let module_dir = Path::new(&config.hybrid_mnt_dir);
        let magic_need_ids: HashSet<String> = magic_queue.iter().cloned().collect();

        if let Err(e) = magic_mount::magic_mount(
            &tempdir,
            module_dir,
            &config.mountsource,
            &config.partitions,
            magic_need_ids,
            !config.disable_umount,
        ) {
            log::error!("Magic Mount critical failure: {:#}", e);
            final_magic_ids.clear();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    if !config.disable_umount
        && let Err(e) = crate::try_umount::commit()
    {
        log::warn!("Final try_umount commit failed: {}", e);
    }

    let mut result_overlay: Vec<String> = final_overlay_ids.into_iter().collect();
    let mut result_magic: Vec<String> = final_magic_ids.into_iter().collect();

    result_overlay.sort();
    result_magic.sort();

    Ok(ExecutionResult {
        overlay_module_ids: result_overlay,
        magic_module_ids: result_magic,
    })
}
