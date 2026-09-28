//! Opt-in integrations that let coding agents report their task-list progress
//! to the pane they run in: an omp extension file, a Claude Code
//! `PostToolUse` hook and a Codex `PostToolUse` hook. Used by `hh progress
//! install|uninstall|status` and by Settings. Nothing is written unless the
//! user asks for it.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};

/// The bundled omp extension; the service ships the same file for bots.
const OMP_EXTENSION: &str = include_str!("../../session-service/bundled/hh-progress.ts");
const OMP_EXTENSION_FILE: &str = "harness-harlot-progress.ts";
/// First line of the installed omp file: the marker plus a hash of the rest,
/// which tells an unmodified older version (`Outdated`) from a user edit.
const OMP_MARKER: &str = "// harness-harlot-progress ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProgressAgent {
    Omp,
    Claude,
    Codex,
}

impl ProgressAgent {
    pub(crate) const ALL: [Self; 3] = [Self::Omp, Self::Claude, Self::Codex];

    /// The CLI spelling: `omp`, `claude` or `codex`.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Omp => "omp",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Omp => "omp",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|agent| agent.id() == value)
    }

    /// Something the user must know after installing, e.g. Codex's hook trust prompt.
    pub(crate) fn note(self) -> Option<&'static str> {
        match self {
            Self::Omp | Self::Claude => None,
            Self::Codex => Some("Codex asks you to trust the new hook the next time it starts"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstallState {
    NotInstalled,
    Installed,
    /// The omp extension file was edited by hand; it is left alone.
    Modified,
    /// An older Harness Harlot version (or another `hh` binary) is installed.
    Outdated,
}

impl InstallState {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::Installed => "installed",
            Self::Modified => "modified",
            Self::Outdated => "outdated",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstallReport {
    pub(crate) path: PathBuf,
    pub(crate) note: Option<&'static str>,
}

/// Where each integration lives and which `hh` the hooks run.
#[derive(Clone, Debug)]
struct Locations {
    omp_agent_dir: PathBuf,
    claude_config_dir: PathBuf,
    codex_home: PathBuf,
    /// Canonical (symlink-free) `hh`, so every install writes the same command.
    hh: PathBuf,
}

impl Locations {
    fn from_env() -> Result<Self> {
        let hh = std::env::current_exe().context("resolve the hh executable")?;
        Self::from_vars(|name| std::env::var_os(name), &hh)
    }

    fn from_vars(var: impl Fn(&str) -> Option<OsString>, hh: &Path) -> Result<Self> {
        let var = |name: &str| var(name).filter(|value| !value.is_empty());
        let home = var("HOME").map(PathBuf::from).context("HOME is not set")?;
        let dir = |variable: &str, default: &str| {
            var(variable).map_or_else(|| home.join(default), PathBuf::from)
        };
        Ok(Self {
            omp_agent_dir: dir("PI_CODING_AGENT_DIR", ".omp/agent"),
            claude_config_dir: dir("CLAUDE_CONFIG_DIR", ".claude"),
            codex_home: dir("CODEX_HOME", ".codex"),
            hh: canonical(hh),
        })
    }

    fn target(&self, agent: ProgressAgent) -> PathBuf {
        match agent {
            ProgressAgent::Omp => self
                .omp_agent_dir
                .join("extensions")
                .join(OMP_EXTENSION_FILE),
            ProgressAgent::Claude => self.claude_config_dir.join("settings.json"),
            ProgressAgent::Codex => self.codex_home.join("hooks.json"),
        }
    }
}

/// `path` without symlinks, or as given when it cannot be resolved.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The file an install writes for `agent`.
pub(crate) fn target_path(agent: ProgressAgent) -> Result<PathBuf> {
    Ok(Locations::from_env()?.target(agent))
}

pub(crate) fn status(agent: ProgressAgent) -> Result<InstallState> {
    status_at(&Locations::from_env()?, agent)
}

pub(crate) fn install(agent: ProgressAgent) -> Result<InstallReport> {
    install_at(&Locations::from_env()?, agent)
}

pub(crate) fn uninstall(agent: ProgressAgent) -> Result<()> {
    uninstall_at(&Locations::from_env()?, agent)
}

fn status_at(locations: &Locations, agent: ProgressAgent) -> Result<InstallState> {
    let path = locations.target(agent);
    match agent {
        ProgressAgent::Omp => Ok(omp_state(read_optional(&path)?.as_deref())),
        ProgressAgent::Claude | ProgressAgent::Codex => {
            let Some(settings) = read_settings(&path)? else {
                return Ok(InstallState::NotInstalled);
            };
            Ok(hook_state(&settings, &HookSpec::new(agent, &locations.hh)))
        }
    }
}

fn install_at(locations: &Locations, agent: ProgressAgent) -> Result<InstallReport> {
    let path = locations.target(agent);
    match agent {
        ProgressAgent::Omp => {
            let existing = read_optional(&path)?;
            match omp_state(existing.as_deref()) {
                InstallState::Installed => {}
                InstallState::Modified => bail!(
                    "refusing to replace modified progress extension {}",
                    path.display()
                ),
                InstallState::NotInstalled | InstallState::Outdated => {
                    write_atomic(&path, omp_file_contents().as_bytes())?;
                }
            }
        }
        ProgressAgent::Claude | ProgressAgent::Codex => {
            let spec = HookSpec::new(agent, &locations.hh);
            let mut settings = read_settings(&path)?.unwrap_or_else(|| Value::Object(Map::new()));
            if hook_state(&settings, &spec) != InstallState::Installed {
                remove_hook(&mut settings, &spec)?;
                add_hook(&mut settings, &spec)?;
                write_json(&path, &settings)?;
            }
        }
    }
    Ok(InstallReport {
        path,
        note: agent.note(),
    })
}

fn uninstall_at(locations: &Locations, agent: ProgressAgent) -> Result<()> {
    let path = locations.target(agent);
    match agent {
        ProgressAgent::Omp => match omp_state(read_optional(&path)?.as_deref()) {
            InstallState::NotInstalled => Ok(()),
            InstallState::Modified => bail!(
                "refusing to remove modified progress extension {}",
                path.display()
            ),
            InstallState::Installed | InstallState::Outdated => fs::remove_file(&path)
                .with_context(|| format!("remove progress extension {}", path.display())),
        },
        ProgressAgent::Claude | ProgressAgent::Codex => {
            let Some(mut settings) = read_settings(&path)? else {
                return Ok(());
            };
            if remove_hook(&mut settings, &HookSpec::new(agent, &locations.hh))? {
                write_json(&path, &settings)?;
            }
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ omp file

/// FNV-1a: a stable content fingerprint, not a security measure.
fn fingerprint(text: &str) -> String {
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

fn omp_file_contents() -> String {
    format!(
        "{OMP_MARKER}{}\n{OMP_EXTENSION}",
        fingerprint(OMP_EXTENSION)
    )
}

fn omp_state(existing: Option<&str>) -> InstallState {
    let Some(existing) = existing else {
        return InstallState::NotInstalled;
    };
    let Some((marker, body)) = existing.split_once('\n') else {
        return InstallState::Modified;
    };
    match marker.strip_prefix(OMP_MARKER) {
        Some(hash) if hash == fingerprint(body) => {
            if body == OMP_EXTENSION {
                InstallState::Installed
            } else {
                InstallState::Outdated
            }
        }
        _ => InstallState::Modified,
    }
}

// ------------------------------------------------------------- hook settings

/// One `PostToolUse` hook entry in Claude's `settings.json` or Codex's `hooks.json`.
struct HookSpec {
    matcher: &'static str,
    command: String,
    /// Canonical `hh` of `command`.
    hh: PathBuf,
    /// Arguments that identify our hook whatever `hh` path it runs.
    suffix: &'static str,
}

impl HookSpec {
    fn new(agent: ProgressAgent, hh: &Path) -> Self {
        let (matcher, suffix) = match agent {
            ProgressAgent::Codex => (
                hh_protocol::CODEX_PROGRESS_HOOK_MATCHER,
                " progress hook codex",
            ),
            ProgressAgent::Claude | ProgressAgent::Omp => (
                hh_protocol::CLAUDE_PROGRESS_HOOK_MATCHER,
                " progress hook claude",
            ),
        };
        Self {
            matcher,
            command: format!("{}{suffix}", shell_quote(&hh.to_string_lossy())),
            hh: canonical(hh),
            suffix,
        }
    }

    fn is_ours(&self, hook: &Value) -> bool {
        hook.get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command.trim_end().ends_with(self.suffix))
    }

    /// Whether `command` runs this `hh`, through whatever symlinked path.
    fn runs_our_hh(&self, command: &str) -> bool {
        if command == self.command {
            return true;
        }
        command
            .trim_end()
            .strip_suffix(self.suffix)
            .and_then(shell_unquote)
            .is_some_and(|program| canonical(Path::new(&program)) == self.hh)
    }
}

/// Single-quotes `text` for POSIX shells.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Reverses `shell_quote`; also accepts a bare word without quoting or escapes.
fn shell_unquote(text: &str) -> Option<String> {
    let text = text.trim();
    if !text.starts_with('\'') {
        return (!text.is_empty() && !text.contains(['"', '\\', '$', '`', ' ', '\'']))
            .then(|| text.to_owned());
    }
    let mut unquoted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(quoted) = rest.strip_prefix('\'') {
        let (literal, after) = quoted.split_once('\'')?;
        unquoted.push_str(literal);
        rest = match after.strip_prefix(r"\'") {
            Some(after) => {
                unquoted.push('\'');
                after
            }
            None => after,
        };
    }
    rest.is_empty().then_some(unquoted)
}

fn post_tool_use(settings: &Value) -> Option<&Vec<Value>> {
    settings.get("hooks")?.get("PostToolUse")?.as_array()
}

fn hook_state(settings: &Value, spec: &HookSpec) -> InstallState {
    let mut state = InstallState::NotInstalled;
    for group in post_tool_use(settings).into_iter().flatten() {
        let matcher = group.get("matcher").and_then(Value::as_str);
        let hooks = group.get("hooks").and_then(Value::as_array);
        for hook in hooks
            .into_iter()
            .flatten()
            .filter(|hook| spec.is_ours(hook))
        {
            let command = hook.get("command").and_then(Value::as_str);
            if matcher == Some(spec.matcher)
                && command.is_some_and(|command| spec.runs_our_hh(command))
            {
                return InstallState::Installed;
            }
            state = InstallState::Outdated;
        }
    }
    state
}

fn add_hook(settings: &mut Value, spec: &HookSpec) -> Result<()> {
    let root = settings
        .as_object_mut()
        .context("settings file is not a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("settings \"hooks\" is not a JSON object")?;
    hooks
        .entry("PostToolUse")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("settings \"hooks.PostToolUse\" is not a JSON array")?
        .push(json!({
            "matcher": spec.matcher,
            "hooks": [{ "type": "command", "command": spec.command }],
        }));
    Ok(())
}

/// Removes every hook of ours, then any matcher group and `PostToolUse` list
/// that removal left empty. Returns whether anything changed.
fn remove_hook(settings: &mut Value, spec: &HookSpec) -> Result<bool> {
    let Some(hooks) = settings.get_mut("hooks") else {
        return Ok(false);
    };
    let hooks = hooks
        .as_object_mut()
        .context("settings \"hooks\" is not a JSON object")?;
    let Some(groups) = hooks.get_mut("PostToolUse") else {
        return Ok(false);
    };
    let groups = groups
        .as_array_mut()
        .context("settings \"hooks.PostToolUse\" is not a JSON array")?;
    let mut changed = false;
    groups.retain_mut(|group| {
        let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = entries.len();
        entries.retain(|hook| !spec.is_ours(hook));
        if entries.len() == before {
            return true;
        }
        changed = true;
        !entries.is_empty()
    });
    if changed && groups.is_empty() {
        hooks.remove("PostToolUse");
    }
    Ok(changed)
}

// --------------------------------------------------------------------- files

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Parses an existing settings file; a file that is not JSON is never rewritten.
fn read_settings(path: &Path) -> Result<Option<Value>> {
    let Some(text) = read_optional(path)? else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(Some(Value::Object(Map::new())));
    }
    let settings: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?;
    ensure!(
        settings.is_object(),
        "{} is not a JSON object",
        path.display()
    );
    Ok(Some(settings))
}

fn write_json(path: &Path, settings: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

/// Replaces `path` through a same-directory temporary and a rename. An existing
/// file keeps its permissions; a new one is owner-only. A symlink (e.g. from a
/// dotfile manager) stays in place and its target is replaced instead.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let resolved;
    let path = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            resolved = fs::canonicalize(path).with_context(|| {
                format!(
                    "{} is a symlink whose target cannot be resolved; fix the link first",
                    path.display()
                )
            })?;
            resolved.as_path()
        }
        _ => path,
    };
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mode = match fs::metadata(path) {
        Ok(metadata) => metadata.permissions().mode() & 0o7777,
        Err(error) if error.kind() == ErrorKind::NotFound => 0o600,
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings");
    let temporary = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        locations: Locations,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("hh-progress-test-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            fs::create_dir_all(&home).unwrap();
            let locations = Locations {
                omp_agent_dir: home.join(".omp/agent"),
                claude_config_dir: home.join(".claude"),
                codex_home: home.join(".codex"),
                hh: PathBuf::from("/Applications/Harness Harlot.app/Contents/MacOS/hh"),
            };
            Self { root, locations }
        }

        fn read_json(&self, agent: ProgressAgent) -> Value {
            serde_json::from_str(&fs::read_to_string(self.locations.target(agent)).unwrap())
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn omp_install_is_idempotent_and_refuses_to_touch_modified_files() {
        let fixture = Fixture::new();
        let omp = ProgressAgent::Omp;
        let path = fixture.locations.target(omp);
        assert_eq!(
            status_at(&fixture.locations, omp).unwrap(),
            InstallState::NotInstalled
        );

        let report = install_at(&fixture.locations, omp).unwrap();
        assert_eq!(report.path, path);
        assert!(path.ends_with(".omp/agent/extensions/harness-harlot-progress.ts"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            status_at(&fixture.locations, omp).unwrap(),
            InstallState::Installed
        );
        install_at(&fixture.locations, omp).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), omp_file_contents());

        let edited = format!("{}// mine\n", omp_file_contents());
        fs::write(&path, &edited).unwrap();
        assert_eq!(
            status_at(&fixture.locations, omp).unwrap(),
            InstallState::Modified
        );
        assert!(install_at(&fixture.locations, omp).is_err());
        assert!(uninstall_at(&fixture.locations, omp).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    }

    #[test]
    fn omp_older_bundled_version_is_outdated_and_replaced() {
        let fixture = Fixture::new();
        let omp = ProgressAgent::Omp;
        let path = fixture.locations.target(omp);
        let old_body = "export default function old() {}\n";
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!("{OMP_MARKER}{}\n{old_body}", fingerprint(old_body)),
        )
        .unwrap();
        assert_eq!(
            status_at(&fixture.locations, omp).unwrap(),
            InstallState::Outdated
        );

        install_at(&fixture.locations, omp).unwrap();
        assert_eq!(
            status_at(&fixture.locations, omp).unwrap(),
            InstallState::Installed
        );
        uninstall_at(&fixture.locations, omp).unwrap();
        assert!(!path.exists());
        uninstall_at(&fixture.locations, omp).unwrap();
    }

    #[test]
    fn omp_install_honors_the_agent_directory_override() {
        let mut fixture = Fixture::new();
        fixture.locations.omp_agent_dir = fixture.root.join("custom-agent");
        let report = install_at(&fixture.locations, ProgressAgent::Omp).unwrap();
        assert_eq!(
            report.path,
            fixture
                .root
                .join("custom-agent/extensions/harness-harlot-progress.ts")
        );
    }

    #[test]
    fn claude_install_creates_an_owner_only_settings_file_with_the_quoted_hook() {
        let fixture = Fixture::new();
        let claude = ProgressAgent::Claude;
        install_at(&fixture.locations, claude).unwrap();
        let path = fixture.locations.target(claude);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fixture.read_json(claude),
            json!({ "hooks": { "PostToolUse": [{
                "matcher": "TodoWrite|TaskCreate|TaskUpdate",
                "hooks": [{
                    "type": "command",
                    "command": "'/Applications/Harness Harlot.app/Contents/MacOS/hh' progress hook claude",
                }],
            }]}})
        );
        assert_eq!(
            status_at(&fixture.locations, claude).unwrap(),
            InstallState::Installed
        );
    }

    #[test]
    fn claude_install_preserves_other_settings_and_hooks_and_uninstall_removes_only_ours() {
        let fixture = Fixture::new();
        let claude = ProgressAgent::Claude;
        let path = fixture.locations.target(claude);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = json!({
            "model": "opus",
            "permissions": { "allow": ["Bash(ls)"] },
            "hooks": {
                "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard" }] }],
                "PostToolUse": [{ "matcher": "Edit", "hooks": [{ "type": "command", "command": "fmt" }] }],
            },
        });
        fs::write(&path, serde_json::to_string(&original).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        install_at(&fixture.locations, claude).unwrap();
        install_at(&fixture.locations, claude).unwrap();
        let installed = fixture.read_json(claude);
        assert_eq!(installed["model"], original["model"]);
        assert_eq!(installed["permissions"], original["permissions"]);
        assert_eq!(
            installed["hooks"]["PreToolUse"],
            original["hooks"]["PreToolUse"]
        );
        let post = installed["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 2, "re-install must not duplicate the hook");
        assert_eq!(post[0], original["hooks"]["PostToolUse"][0]);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );

        uninstall_at(&fixture.locations, claude).unwrap();
        assert_eq!(fixture.read_json(claude), original);
        assert_eq!(
            status_at(&fixture.locations, claude).unwrap(),
            InstallState::NotInstalled
        );
    }

    #[test]
    fn a_hook_for_another_hh_path_is_outdated_and_replaced_on_install() {
        let fixture = Fixture::new();
        let codex = ProgressAgent::Codex;
        let path = fixture.locations.target(codex);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            json!({ "hooks": { "PostToolUse": [{
                "matcher": "^update_plan$",
                "hooks": [
                    { "type": "command", "command": "'/old/hh' progress hook codex" },
                    { "type": "command", "command": "notify" },
                ],
            }]}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            status_at(&fixture.locations, codex).unwrap(),
            InstallState::Outdated
        );

        let report = install_at(&fixture.locations, codex).unwrap();
        assert!(report.note.is_some());
        let installed = fixture.read_json(codex);
        let post = installed["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(
            post[0]["hooks"],
            json!([{ "type": "command", "command": "notify" }])
        );
        assert_eq!(
            post[1]["hooks"][0]["command"],
            "'/Applications/Harness Harlot.app/Contents/MacOS/hh' progress hook codex"
        );
        assert_eq!(post.len(), 2);
        assert_eq!(
            status_at(&fixture.locations, codex).unwrap(),
            InstallState::Installed
        );
    }

    #[test]
    fn invalid_settings_are_never_overwritten() {
        let fixture = Fixture::new();
        let claude = ProgressAgent::Claude;
        let path = fixture.locations.target(claude);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert!(install_at(&fixture.locations, claude).is_err());
        assert!(uninstall_at(&fixture.locations, claude).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn uninstall_without_settings_creates_nothing() {
        let fixture = Fixture::new();
        for agent in ProgressAgent::ALL {
            uninstall_at(&fixture.locations, agent).unwrap();
            assert!(!fixture.locations.target(agent).exists());
        }
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(shell_quote("/a b/it's/hh"), r"'/a b/it'\''s/hh'");
        assert_eq!(
            shell_unquote(&shell_quote("/a b/it's/hh")).as_deref(),
            Some("/a b/it's/hh")
        );
        assert_eq!(
            shell_unquote("/usr/local/bin/hh").as_deref(),
            Some("/usr/local/bin/hh")
        );
        assert_eq!(shell_unquote("'/a' b"), None);
        assert_eq!(shell_unquote("$(evil)"), None);
    }

    #[test]
    fn a_todo_write_only_claude_hook_is_outdated_and_replaced() {
        let fixture = Fixture::new();
        let claude = ProgressAgent::Claude;
        let path = fixture.locations.target(claude);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let command = "'/Applications/Harness Harlot.app/Contents/MacOS/hh' progress hook claude";
        fs::write(
            &path,
            json!({ "hooks": { "PostToolUse": [{
                "matcher": "TodoWrite",
                "hooks": [{ "type": "command", "command": command }],
            }]}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            status_at(&fixture.locations, claude).unwrap(),
            InstallState::Outdated
        );
        install_at(&fixture.locations, claude).unwrap();
        assert_eq!(
            fixture.read_json(claude),
            json!({ "hooks": { "PostToolUse": [{
                "matcher": hh_protocol::CLAUDE_PROGRESS_HOOK_MATCHER,
                "hooks": [{ "type": "command", "command": command }],
            }]}})
        );
    }

    #[test]
    fn install_writes_through_a_symlinked_settings_file() {
        let fixture = Fixture::new();
        let dotfiles = fixture.root.join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        for (agent, name) in [
            (ProgressAgent::Claude, "claude-settings.json"),
            (ProgressAgent::Codex, "codex-hooks.json"),
        ] {
            let target = dotfiles.join(name);
            fs::write(&target, r#"{ "model": "opus" }"#).unwrap();
            let link = fixture.locations.target(agent);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();

            install_at(&fixture.locations, agent).unwrap();
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let installed: Value =
                serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
            assert_eq!(installed["model"], "opus");
            assert_eq!(
                status_at(&fixture.locations, agent).unwrap(),
                InstallState::Installed
            );

            uninstall_at(&fixture.locations, agent).unwrap();
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let removed: Value =
                serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
            assert_eq!(removed["model"], "opus");
            assert_eq!(
                status_at(&fixture.locations, agent).unwrap(),
                InstallState::NotInstalled
            );
        }

        // A dangling link is refused rather than replaced by a plain file.
        let dangling = fixture.root.join("elsewhere/settings.json");
        let claude = fixture.locations.target(ProgressAgent::Claude);
        fs::remove_file(&claude).unwrap();
        std::os::unix::fs::symlink(&dangling, &claude).unwrap();
        assert!(install_at(&fixture.locations, ProgressAgent::Claude).is_err());
        assert!(
            fs::symlink_metadata(&claude)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_hook_through_a_symlink_to_the_same_hh_counts_as_installed() {
        let mut fixture = Fixture::new();
        let app = fixture.root.join("Harness Harlot.app/Contents/MacOS");
        fs::create_dir_all(&app).unwrap();
        let real = app.join("hh");
        fs::write(&real, "").unwrap();
        let bin = fixture.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let link = bin.join("hh");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Running `hh` through the link installs the canonical path.
        let home = fixture.root.join("home");
        fixture.locations =
            Locations::from_vars(|name| (name == "HOME").then(|| home.clone().into()), &link)
                .unwrap();
        let codex = ProgressAgent::Codex;
        install_at(&fixture.locations, codex).unwrap();
        let canonical_real = fs::canonicalize(&real).unwrap();
        assert_eq!(
            fixture.read_json(codex)["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            format!(
                "{} progress hook codex",
                shell_quote(&canonical_real.to_string_lossy())
            )
        );

        // An entry written with the symlinked path is still ours and current.
        let path = fixture.locations.target(codex);
        fs::write(
            &path,
            json!({ "hooks": { "PostToolUse": [{
                "matcher": hh_protocol::CODEX_PROGRESS_HOOK_MATCHER,
                "hooks": [{
                    "type": "command",
                    "command": format!("{} progress hook codex", shell_quote(&link.to_string_lossy())),
                }],
            }]}})
            .to_string(),
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();
        assert_eq!(
            status_at(&fixture.locations, codex).unwrap(),
            InstallState::Installed
        );
        install_at(&fixture.locations, codex).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn locations_honor_config_directory_overrides() {
        let hh = Path::new("/opt/hh");
        let defaults = Locations::from_vars(
            |name| (name == "HOME").then(|| OsString::from("/home/me")),
            hh,
        )
        .unwrap();
        assert_eq!(
            defaults.target(ProgressAgent::Claude),
            Path::new("/home/me/.claude/settings.json")
        );
        assert_eq!(
            defaults.target(ProgressAgent::Codex),
            Path::new("/home/me/.codex/hooks.json")
        );
        let custom = Locations::from_vars(
            |name| match name {
                "HOME" => Some("/home/me".into()),
                "CLAUDE_CONFIG_DIR" => Some("/cfg/claude".into()),
                "CODEX_HOME" => Some("/cfg/codex".into()),
                "PI_CODING_AGENT_DIR" => Some("/cfg/omp".into()),
                _ => None,
            },
            hh,
        )
        .unwrap();
        assert_eq!(
            custom.target(ProgressAgent::Claude),
            Path::new("/cfg/claude/settings.json")
        );
        assert_eq!(
            custom.target(ProgressAgent::Codex),
            Path::new("/cfg/codex/hooks.json")
        );
        assert_eq!(
            custom.target(ProgressAgent::Omp),
            Path::new("/cfg/omp/extensions/harness-harlot-progress.ts")
        );
    }
}
