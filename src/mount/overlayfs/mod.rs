#![allow(clippy::module_inception)]
// Copyright 2025 Meta-Hybrid Mount Authors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod overlayfs;
pub mod utils;

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::{Result, bail};

use crate::defs;

#[allow(dead_code)]
pub fn mount_systemlessly(
    module_id: HashSet<String>,
    extra_partitions: &[String],
    mount_source: &str,
) -> Result<()> {
    let module_dir = Path::new(defs::MODULES_DIR);
    let dir = module_dir.read_dir();
    let Ok(dir) = dir else {
        bail!("open {} failed", defs::MODULES_DIR);
    };

    let mut system_lowerdir: Vec<String> = Vec::new();

    let partition = vec!["vendor", "product", "system_ext", "odm", "oem"];
    let mut partition_lowerdir: HashMap<String, Vec<String>> = HashMap::new();
    for ele in &partition {
        partition_lowerdir.insert((*ele).to_string(), Vec::new());
    }
    for p in extra_partitions {
        partition_lowerdir.insert(p.clone(), Vec::new());
    }

    for entry in dir.flatten() {
        let module = entry.path();
        if !module.is_dir() {
            continue;
        }
        if let Some(module_name) = module.file_name() {
            let real_module_path = module_dir.join(module_name);

            let disabled = real_module_path.join(defs::DISABLE_FILE_NAME).exists();

            if disabled {
                log::info!("module: {} is disabled, ignore!", module.display());
                continue;
            }
            if !module_id.contains(&module_name.to_string_lossy().to_string()) {
                continue;
            }
        }

        let skip_mount = module.join(defs::SKIP_MOUNT_FILE_NAME).exists();
        if skip_mount {
            log::info!("module: {} skip_mount exist, skip!", module.display());
            continue;
        }

        let module_system = Path::new(&module).join("system");
        if module_system.is_dir() {
            system_lowerdir.push(format!("{}", module_system.display()));
        }

        for part in &partition {
            let part_path = Path::new(&module).join(part);
            #[allow(clippy::collapsible_if)]
            if part_path.is_dir() {
                if let Some(v) = partition_lowerdir.get_mut(*part) {
                    v.push(format!("{}", part_path.display()));
                }
            }
        }
    }

    // system 分区通常不是软链接，且内容复杂，保持原有的根目录挂载方式
    if let Err(e) = mount_partition("system", &system_lowerdir, mount_source) {
        log::warn!("mount system failed: {:#}", e);
    }

    // 对于 vendor, product 等分区，使用 mountify 的子目录挂载策略 (Controlled Depth)
    for (k, v) in partition_lowerdir {
        if let Err(e) = mount_partition_subdirs(&k, &v, mount_source) {
            log::warn!("mount {k} failed: {:#}", e);
        }
    }

    Ok(())
}

/// 传统的挂载方式，直接挂载到分区根目录
/// 适用于 system 分区
#[allow(dead_code)]
fn mount_partition<S>(partition_name: S, lowerdir: &Vec<String>, mount_source: &str) -> Result<()>
where
    S: AsRef<str>,
{
    let partition_name = partition_name.as_ref();
    if lowerdir.is_empty() {
        // log::warn!("partition: {partition_name} lowerdir is empty");
        return Ok(());
    }

    let partition = format!("/{partition_name}");

    // 如果目标是软链接，这种全目录挂载会导致原有链接丢失（被 overlay 覆盖），
    // 这里的检查会跳过挂载。但在 mount_partition_subdirs 中我们会处理这种情况。
    if Path::new(&partition).read_link().is_ok() {
        log::warn!("partition: {partition} is a symlink, skipping root mount");
        return Ok(());
    }

    let mut workdir = None;
    let mut upperdir = None;
    let system_rw_dir = Path::new(defs::SYSTEM_RW_DIR);
    if system_rw_dir.exists() {
        workdir = Some(system_rw_dir.join(partition_name).join("workdir"));
        upperdir = Some(system_rw_dir.join(partition_name).join("upperdir"));
    }

    overlayfs::mount_overlay(&partition, lowerdir, workdir, upperdir, mount_source)
}

/// 移植自 Mountify 的子目录挂载策略 (Controlled Depth)
/// 1. 自动检测挂载基点（解决 /vendor -> /system/vendor 软链接问题）
/// 2. 仅挂载模块中存在的子目录（如 /vendor/bin），避免覆盖父目录软链接
fn mount_partition_subdirs(
    partition_name: &str,
    lowerdirs: &Vec<String>,
    mount_source: &str,
) -> Result<()> {
    if lowerdirs.is_empty() {
        return Ok(());
    }

    // 1. 确定挂载基点 (Root Detection)
    // 逻辑参考 mountify: 如果 /partition 是软链接 且 /system/partition 是实体目录，则挂载到 /system/partition
    let p_root = Path::new("/").join(partition_name);
    let p_sys = Path::new("/system").join(partition_name);

    let target_base = if p_root.is_symlink() && !p_sys.is_symlink() && p_sys.exists() {
        log::info!(
            "{} is a symlink, switching mount base to {}",
            p_root.display(),
            p_sys.display()
        );
        p_sys
    } else {
        p_root
    };

    // 2. 扫描所有模块中包含的该分区的子目录 (收集去重)
    let mut subdirs = HashSet::new();
    for dir in lowerdirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        subdirs.insert(name.to_string());
                    }
                }
            }
        }
    }

    // 3. 对每个子目录分别进行挂载
    for sub in subdirs {
        let mount_point = target_base.join(&sub);

        // 如果设备上不存在该目标目录，则跳过（OverlayFS 需要挂载点存在）
        if !mount_point.exists() {
            log::debug!("mount point {} does not exist, skipping", mount_point.display());
            continue;
        }

        // 收集该子目录对应的所有模块路径
        let mut sub_lower: Vec<String> = Vec::new();
        for dir in lowerdirs {
            let candidate = Path::new(dir).join(&sub);
            if candidate.is_dir() {
                sub_lower.push(candidate.to_string_lossy().to_string());
            }
        }

        if sub_lower.is_empty() {
            continue;
        }

        // 配置 RW 目录（如果开启）
        // 注意：为每个子目录分配独立的 workdir/upperdir
        let mut workdir = None;
        let mut upperdir = None;
        let system_rw_dir = Path::new(defs::SYSTEM_RW_DIR);
        if system_rw_dir.exists() {
            let part_rw = system_rw_dir.join(partition_name).join(&sub);
            workdir = Some(part_rw.join("workdir"));
            upperdir = Some(part_rw.join("upperdir"));
        }

        let mount_point_str = mount_point.to_string_lossy().to_string();
        
        // 调用底层的 mount_overlay
        if let Err(e) = overlayfs::mount_overlay(
            &mount_point_str,
            &sub_lower,
            workdir,
            upperdir,
            mount_source,
        ) {
            log::warn!("mount subdir {} failed: {:#}", mount_point_str, e);
        }
    }

    Ok(())
}
