// Copyright 2026 Hybrid Mount Developers
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::Result;
use rustix::mount::{mount, MountFlags};
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

fn execute_overlay_op(
    op: &crate::core::planner::OverlayOperation,
    config: &config::Config,
    final_overlay_ids: &mut HashSet<String>,
    final_magic_ids: &mut HashSet<String>,
) {
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
            if !config.disable_umount
                && let Err(e) = crate::try_umount::send_unmountable(&op.target)
            {
                log::warn!("Failed to schedule unmount for {}: {}", op.target, e);
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

pub fn execute(plan: &MountPlan, config: &config::Config) -> Result<ExecutionResult> {
    let mut final_magic_ids: HashSet<String> = plan.magic_module_ids.iter().cloned().collect();
    let mut final_overlay_ids: HashSet<String> = HashSet::new();

    log::info!(">> Phase 1: OverlayFS Execution...");

    // [Step 0] Pre-analysis of symlinks in /system
    let partitions_check_list = vec!["vendor", "product", "system_ext", "odm", "oem"];
    let mut system_symlinks_to_restore = HashMap::new();
    
    for p in partitions_check_list {
        let path = Path::new("/system").join(p);
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.is_symlink() {
                if let Ok(target) = std::fs::canonicalize(&path) {
                    log::debug!("Detected /system symlink: {} -> {}", path.display(), target.display());
                    system_symlinks_to_restore.insert(path, target);
                }
            }
        }
    }

    let (system_ops, other_ops): (Vec<_>, Vec<_>) = plan
        .overlay_ops
        .iter()
        .partition(|op| op.partition_name == "system");

    // [Step 1] Mount System Overlay
    for op in system_ops {
        execute_overlay_op(op, config, &mut final_overlay_ids, &mut final_magic_ids);
    }

    // [Step 2] Mount Other Overlays (Vendor, etc.)
    for op in other_ops {
        execute_overlay_op(op, config, &mut final_overlay_ids, &mut final_magic_ids);
    }

    // [Step 3] Restore Symlinks (Bind Mount)
    for (link_path, target_path) in system_symlinks_to_restore {
        if let Ok(meta) = std::fs::symlink_metadata(&link_path) {
            if meta.is_dir() && !meta.is_symlink() {
                log::info!(
                    "Restoring masked symlink via bind-mount: {} -> {}",
                    link_path.display(),
                    target_path.display()
                );
                
                // [FIX] fstype="" (Arg), data=None (Into<Option<&CStr>>)
                if let Err(e) = mount(
                    &target_path,
                    &link_path,
                    "",
                    MountFlags::BIND | MountFlags::REC,
                    None,
                ) {
                    log::warn!("Failed to restore symlink for {}: {}", link_path.display(), e);
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
