use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

const SKILL: &str = include_str!("../../assets/skills/harness-harlot/SKILL.md");
const SKILL_NAME: &str = "harness-harlot";

pub(crate) fn source_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets/skills")
        .join(SKILL_NAME)
        .join("SKILL.md")
}

pub(crate) fn install_default() -> Result<Vec<PathBuf>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    install(&home)
}

pub(crate) fn install(home: &Path) -> Result<Vec<PathBuf>> {
    let roots = [
        home.join(".claude/skills"),
        home.join(".codex/skills"),
        home.join(".pi/agent/skills"),
    ];
    let mut installed = Vec::with_capacity(roots.len());
    for root in roots {
        let directory = root.join(SKILL_NAME);
        fs::create_dir_all(&directory)
            .with_context(|| format!("create skill directory {}", directory.display()))?;
        let path = directory.join("SKILL.md");
        if path.exists() {
            let existing = fs::read_to_string(&path)
                .with_context(|| format!("read existing skill {}", path.display()))?;
            ensure!(
                existing == SKILL,
                "refusing to replace modified skill {}",
                path.display()
            );
        } else {
            fs::write(&path, SKILL).with_context(|| format!("install skill {}", path.display()))?;
        }
        installed.push(path);
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_is_idempotent_and_preserves_modified_skills() {
        let root = std::env::temp_dir().join(format!("hh-skill-test-{}", uuid::Uuid::new_v4()));
        let paths = install(&root).unwrap();
        assert_eq!(paths.len(), 3);
        assert!(
            paths
                .iter()
                .all(|path| fs::read_to_string(path).unwrap() == SKILL)
        );
        assert_eq!(install(&root).unwrap(), paths);

        fs::write(&paths[0], "locally modified").unwrap();
        assert!(install(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
