// Copyright 2025 Meta-Hybrid Mount Authors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use rayon::prelude::*;
use serde::Serialize;
use walkdir::WalkDir;

use crate::{
    conf::config,
    core::inventory::{Module, MountMode},
    defs,
};

#[derive(Debug, Clone)]
pub struct OverlayOperation {
    pub partition_name: String,
    pub target: String,
    pub lowerdirs: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct MountPlan {
    pub overlay_ops: Vec<OverlayOperation>,
    pub overlay_module_ids: Vec<String>,
    pub magic_module_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConflictEntry {
    pub partition: String,
    pub relative_path: String,
    pub contending_modules: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ConflictReport {
    pub details: Vec<ConflictEntry>,
}

impl MountPlan {
    pub fn analyze_conflicts(&self) -> ConflictReport {
        let mut conflicts: Vec<ConflictEntry> = self
            .overlay_ops
            .par_iter()
            .flat_map(|op| {
                let mut local_conflicts = Vec::new();
                let mut file_map: HashMap<String, Vec<String>> = HashMap::new();

                for layer_path in &op.lowerdirs {
                    let module_id = crate::utils::extract_module_id(layer_path)
                        .unwrap_or_else(|| "UNKNOWN".into());

                    for entry in WalkDir::new(layer_path).min_depth(1).into_iter().flatten() {
                        if !entry.file_type().is_file() {
                            continue;
                        }

                        if let Ok(rel) = entry.path().strip_prefix(layer_path) {
                            let rel_str = rel.to_string_lossy().to_string();
                            file_map.entry(rel_str).or_default().push(module_id.clone());
                        }
                    }
                }

                for (rel_path, modules) in file_map {
                    if modules.len() > 1 {
                        local_conflicts.push(ConflictEntry {
                            partition: op.partition_name.clone(),
                            relative_path: rel_path,
                            contending_modules: modules,
                        });
                    }
                }

                local_conflicts
            })
            .collect();

        conflicts.sort_by(|a, b| {
            a.partition
                .cmp(&b.partition)
                .then_with(|| a.relative_path.cmp(&b.relative_path))
        });

        ConflictReport { details: conflicts }
    }
}

struct ModuleContribution {
    id: String,
    overlays: Vec<(String, PathBuf)>,
    magic: bool,
}

pub fn generate(
    config: &config::Config,
    modules: &[Module],
    storage_root: &Path,
) -> Result<MountPlan> {
    let mut plan = MountPlan::default();

    // 预先识别哪些 /system/xxx 是软链接，需要重定向处理
    // 例如：如果 /system/vendor -> /vendor，我们记录 "vendor"
    let mut symlink_partitions = HashSet::new();
    let check_list = vec!["vendor", "product", "system_ext", "odm", "oem"];
    for p in &check_list {
        let sys_path = Path::new("/system").join(p);
        if let Ok(meta) = fs::symlink_metadata(&sys_path) {
            if meta.file_type().is_symlink() {
                symlink_partitions.insert(p.to_string());
            }
        }
    }

    let mut target_partitions = defs::BUILTIN_PARTITIONS.to_vec();
    target_partitions.extend(config.partitions.iter().map(|s| s.as_str()));

    let contributions: Vec<Option<ModuleContribution>> = modules
        .par_iter()
        .map(|module| {
            let mut content_path = storage_root.join(&module.id);
            if !content_path.exists() {
                content_path = module.source_path.clone();
            }
            if !content_path.exists() {
                return None;
            }

            let mut contrib = ModuleContribution {
                id: module.id.clone(),
                overlays: Vec::new(),
                magic: false,
            };
            let mut has_any_action = false;

            if let Ok(entries) = fs::read_dir(&content_path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let dir_name = entry.file_name().to_string_lossy().to_string();

                    // [Modified Logic] Smart Redirection
                    // 1. Check if this is "system" directory
                    if dir_name == "system" {
                        // Always add 'system' itself to overlay
                        if target_partitions.contains(&"system") {
                            let mode = module.rules.get_mode("system");
                            if mode == MountMode::Overlay {
                                contrib.overlays.push(("system".to_string(), path.clone()));
                                has_any_action = true;
                            }
                        }

                        // 2. Look inside 'system' for symlink partitions (e.g. system/vendor)
                        // If found, ADD them to the target partition's overlay list too.
                        for p in &symlink_partitions {
                            let sub_path = path.join(p);
                            if sub_path.is_dir() && has_files(&sub_path) && target_partitions.contains(&p.as_str()) {
                                let mode = module.rules.get_mode(p);
                                if mode == MountMode::Overlay {
                                    // Redirect: module/system/vendor -> partition: vendor
                                    contrib.overlays.push((p.clone(), sub_path));
                                    has_any_action = true;
                                }
                            }
                        }
                        continue;
                    }

                    // Standard processing for non-system partitions (e.g. module/vendor)
                    if !target_partitions.contains(&dir_name.as_str()) {
                        continue;
                    }
                    if !has_files(&path) {
                        continue;
                    }

                    let mode = module.rules.get_mode(&dir_name);
                    match mode {
                        MountMode::Overlay => {
                            contrib.overlays.push((dir_name, path));
                            has_any_action = true;
                        }
                        MountMode::Magic => {
                            contrib.magic = true;
                            has_any_action = true;
                        }
                        MountMode::Ignore => {
                            log::debug!("Ignoring {}/{} per rule", module.id, dir_name);
                        }
                    }
                }
            }

            if has_any_action { Some(contrib) } else { None }
        })
        .collect();

    let mut overlay_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut overlay_ids = HashSet::new();
    let mut magic_ids = HashSet::new();

    for contrib in contributions.into_iter().flatten() {
        if contrib.magic {
            magic_ids.insert(contrib.id.clone());
        }
        for (part, path) in contrib.overlays {
            overlay_groups.entry(part).or_default().push(path);
            overlay_ids.insert(contrib.id.clone());
        }
    }

    for (part, layers) in overlay_groups {
        let initial_target_path = format!("/{}", part);
        let target_path_obj = Path::new(&initial_target_path);

        // Resolve real path. If /vendor is a symlink to /system/vendor, we want to mount on the REAL dir (if possible)
        // or adhere to the logic that /vendor is the mount point.
        let resolved_target = if target_path_obj.exists() {
            match target_path_obj.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            }
        } else {
            continue;
        };

        if !resolved_target.is_dir() {
            continue;
        }

        plan.overlay_ops.push(OverlayOperation {
            partition_name: part,
            target: resolved_target.to_string_lossy().to_string(),
            lowerdirs: layers,
        });
    }

    plan.overlay_module_ids = overlay_ids.into_iter().collect();
    plan.magic_module_ids = magic_ids.into_iter().collect();
    plan.overlay_module_ids.sort();
    plan.magic_module_ids.sort();

    Ok(plan)
}

fn has_files(path: &Path) -> bool {
    if let Ok(entries) = fs::read_dir(path)
        && entries.flatten().next().is_some()
    {
        return true;
    }
    false
}
