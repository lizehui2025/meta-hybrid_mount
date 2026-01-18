#![allow(clippy::module_inception)]
// Copyright 2025 Meta-Hybrid Mount Authors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod overlayfs;
pub mod utils;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
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

    // 定义需要特别处理的分区，这些通常在 /system 下是软链接
    let partition = vec!["vendor", "product", "system_ext", "odm", "oem"];
    let mut partition_lowerdir: HashMap<String, Vec<String>> = HashMap::new();
    for ele in &partition {
        partition_lowerdir.insert((*ele).to_string(), Vec::new());
    }
    for p in extra_partitions {
        partition_lowerdir.insert(p.clone(), Vec::new());
    }

    // 1. 预先扫描 /system 下的软链接状态
    // 如果 /system/vendor 是个指向 /vendor 的软链接，我们需要记录下来
    let mut system_symlinks: HashMap<String, PathBuf> = HashMap::new();
    for part in &partition {
        let p = Path::new("/system").join(part);
        // 使用 symlink_metadata 检查是否为软链接
        if let Ok(meta) = std::fs::symlink_metadata(&p) {
            if meta.is_symlink() {
                // 解析出绝对路径目标 (例如 /vendor)
                if let Ok(target) = std::fs::canonicalize(&p) {
                    log::debug!("Detected symlink: {} -> {}", p.display(), target.display());
                    system_symlinks.insert(part.to_string(), target);
                }
            }
        }
    }

    // 收集模块路径
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

    // 2. 挂载 System 分区 Overlay
    if let Err(e) = mount_partition("system", &system_lowerdir, mount_source) {
        log::warn!("mount system failed: {:#}", e);
    }

    // 3. 执行软链接恢复操作 (Symlink Restoration)
    // 如果 System Overlay 导致原有的软链接（如 /system/vendor）被掩盖变成了一个普通目录，
    // 我们需要通过 bind mount 将其恢复指向原始目标（如 /vendor）。
    for (part, target) in system_symlinks {
        let mount_point = Path::new("/system").join(&part);
        
        // 检查当前状态：如果是目录且不再是软链接，说明被 Overlay 覆盖了
        let is_masked = if let Ok(meta) = std::fs::symlink_metadata(&mount_point) {
            meta.is_dir() && !meta.is_symlink()
        } else {
            false
        };

        if is_masked {
            log::info!(
                "Restoring masked symlink for {}: {} -> {}", 
                part, 
                mount_point.display(), 
                target.display()
            );
            // 将原始目标 (如 /vendor) bind mount 到被覆盖的挂载点 (如 /system/vendor)
            // 这样 /system/vendor 再次指向 /vendor 的内容 (其中包含了后续的 vendor overlay)
            if let Err(e) = overlayfs::bind_mount(&target, &mount_point) {
                log::warn!("Failed to restore symlink for {}: {:#}", part, e);
            }
        }
    }

    // 4. 挂载其他分区 (Vendor, Product 等)
    // 使用子目录挂载策略，确保不破坏根目录结构
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
        return Ok(());
    }

    let partition = format!("/{partition_name}");

    // 如果目标本身就是软链接，且我们还没挂载 overlay，这里直接挂载会失败或产生未预期行为。
    // 通常 system 不是软链接，但以防万一。
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

/// 子目录挂载策略 (Controlled Depth)
/// 移植自 Mountify，用于处理 Vendor 等分区，避免覆盖父级软链接
fn mount_partition_subdirs(
    partition_name: &str,
    lowerdirs: &Vec<String>,
    mount_source: &str,
) -> Result<()> {
    if lowerdirs.is_empty() {
        return Ok(());
    }

    // 1. 确定挂载基点
    // 如果 /vendor 是软链接 (指向 /system/vendor)，则尝试切换基点。
    // 但通常 SAR 设备是 /system/vendor -> /vendor，这在上面 restore_symlinks 中处理。
    // 这里处理的是反向情况或特殊的 legacy 设备。
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

    // 2. 扫描模块中的子目录
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

    // 3. 逐个挂载子目录
    for sub in subdirs {
        let mount_point = target_base.join(&sub);

        if !mount_point.exists() {
            log::debug!("mount point {} does not exist, skipping", mount_point.display());
            continue;
        }

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

        let mut workdir = None;
        let mut upperdir = None;
        let system_rw_dir = Path::new(defs::SYSTEM_RW_DIR);
        if system_rw_dir.exists() {
            let part_rw = system_rw_dir.join(partition_name).join(&sub);
            workdir = Some(part_rw.join("workdir"));
            upperdir = Some(part_rw.join("upperdir"));
        }

        let mount_point_str = mount_point.to_string_lossy().to_string();
        
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
