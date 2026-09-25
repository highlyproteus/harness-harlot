use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

pub(crate) fn reveal_in_file_manager(path: &Path) -> Result<()> {
    let (program, arguments) = reveal_command(path)?;
    let status = Command::new(program)
        .args(&arguments)
        .status()
        .with_context(|| format!("open file manager for {}", path.display()))?;
    ensure!(
        status.success(),
        "file manager command failed with {status}"
    );
    Ok(())
}

fn reveal_command(path: &Path) -> Result<(&'static str, Vec<PathBuf>)> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    #[cfg(target_os = "macos")]
    {
        Ok(("open", vec![PathBuf::from("-R"), path]))
    }
    #[cfg(target_os = "linux")]
    {
        let directory = if path.is_dir() {
            path
        } else {
            path.parent()
                .context("file path has no parent directory")?
                .to_owned()
        };
        Ok(("xdg-open", vec![directory]))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("revealing files is unsupported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_uses_an_absolute_target() {
        let (_, arguments) = reveal_command(Path::new("relative/image.png")).unwrap();
        let target = arguments.last().unwrap();
        assert!(target.is_absolute());
    }
}
