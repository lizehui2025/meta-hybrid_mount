// src/core/ops/merge.rs
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{Result, Context};
use crate::core::inventory::Module;
use walkdir::WalkDir;

/// 将所有模块合并到一个单一的目录中
pub fn merge_modules(
    modules: &[Module], 
    merge_target: &Path
) -> Result<Module> { // 返回一个合成的"超级模块"
    
    log::info!(">> Starting Pre-merge: {} modules -> {:?}", modules.len(), merge_target);

    // 确保目标目录存在且为空（或者是一个新的临时目录）
    if merge_target.exists() {
        fs::remove_dir_all(merge_target).with_context(|| "Failed to clean merge target")?;
    }
    fs::create_dir_all(merge_target).with_context(|| "Failed to create merge target")?;

    // 按照优先级顺序（从低到高）遍历模块
    // 注意：假设输入的 modules 已经按优先级排序（通常由 inventory::scan 保证）
    // 如果 modules[0] 是最高优先级，则需要 modules.iter().rev()
    for module in modules {
        let source_path = &module.source_path;
        if !source_path.exists() { continue; }

        log::debug!("Merging module: {}", module.id);
        
        // 递归复制文件
        for entry in WalkDir::new(source_path).into_iter().filter_map(|e| e.ok()) {
            let relative_path = match entry.path().strip_prefix(source_path) {
                Ok(p) => p,
                Err(_) => continue,
            };
            
            if relative_path.as_os_str().is_empty() { continue; }

            let target_path = merge_target.join(relative_path);

            if entry.file_type().is_dir() {
                fs::create_dir_all(&target_path)?;
            } else {
                // 如果是文件或符号链接
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                
                // 简单的覆盖策略：后来的（高优先级）覆盖先来的
                // 生产环境建议使用 fs::copy 或硬链接 (fs::hard_link) 以节省空间
                // 注意：硬链接不能跨分区
                if target_path.exists() {
                    let _ = fs::remove_file(&target_path);
                }
                
                // 这里使用 copy 模拟 tmpfs/ext4 行为
                fs::copy(entry.path(), &target_path)
                    .with_context(|| format!("Failed to copy {:?}", entry.path()))?;
            }
        }
    }

    // 构造一个"超级模块"对象返回
    Ok(Module {
        id: "merged_super_layer".to_string(),
        source_path: merge_target.to_path_buf(),
        // 继承合并后的属性，或者使用默认属性
        rules: crate::core::inventory::model::MountRules::default(), 
        // 其他字段根据你的 Module 结构体填充
        ..modules[0].clone() // 偷懒做法：克隆第一个模块的元数据，然后修改路径
    })
}
