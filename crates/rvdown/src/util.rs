//! 小工具：路径输入清洗（终端里拖进来的路径常带引号与首尾空白）。

use std::path::PathBuf;

/// 去掉首尾空白与成对引号；不做任何存在性检查（调用方决定如何报错）。
pub fn parse_path_input(value: &str) -> PathBuf {
    let trimmed = value.trim();
    let unquoted = if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        trimmed
            .get(1..trimmed.len().saturating_sub(1))
            .unwrap_or(trimmed)
    } else {
        trimmed
    };
    PathBuf::from(unquoted)
}

/// 流式算文件 MD5（大文件不整读进内存）。
pub async fn md5_file(path: &std::path::Path) -> std::io::Result<String> {
    use md5::{Digest, Md5};
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Md5::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}
