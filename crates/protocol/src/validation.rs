//! Input validation shared by the desktop UI and the session service.

use thiserror::Error;

use crate::{
    DEFAULT_BROWSER_URL, MAX_BROWSER_URL_LEN, MAX_SSH_HOST_LEN, MAX_SSH_INPUT_LEN,
    MAX_WORKSPACE_DIR_BYTES,
};

/// A user-facing validation failure for one protocol input.
///
/// Every variant's [`std::fmt::Display`] message is part of the stable
/// user-visible contract; tests assert on the exact strings.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ValidationError {
    #[error("working directory cannot be empty")]
    EmptyWorkspaceDir,
    #[error("working directory must be absolute")]
    RelativeWorkspaceDir,
    #[error("working directory exceeds the {MAX_WORKSPACE_DIR_BYTES}-byte limit")]
    WorkspaceDirTooLong,
    #[error("working directory may not contain control characters")]
    WorkspaceDirControlCharacters,
    #[error("tmux {label} target must be an opaque numeric {label} ID")]
    TmuxTargetId { label: &'static str },
    #[error("SSH destination or command is too long")]
    SshInputTooLong,
    #[error("SSH host, alias, or command is required")]
    SshInputRequired,
    #[error("SSH destination or command may not contain control characters")]
    SshInputControlCharacters,
    #[error("Enter one destination or paste `ssh <destination>` without options or extra commands")]
    SshInputForm,
    #[error("SSH host, alias, or destination is required")]
    SshHostRequired,
    #[error("SSH destination is too long")]
    SshHostTooLong,
    #[error("SSH destination must contain at most one non-empty `user@` prefix")]
    SshUserPrefix,
    #[error("SSH user may contain only letters, numbers, dots, underscores, and hyphens")]
    SshUserCharacters,
    #[error("SSH host or alias is required")]
    SshHostEmpty,
    #[error("SSH host or alias must start with a letter or number")]
    SshHostStart,
    #[error("SSH host or alias may contain only letters, numbers, dots, underscores, and hyphens")]
    SshHostCharacters,
}

#[derive(Debug, Error)]
pub enum BrowserUrlError {
    #[error("browser URL cannot be empty")]
    Empty,
    #[error("browser URL exceeds the {MAX_BROWSER_URL_LEN}-byte limit")]
    TooLong,
    #[error("browser URL may not contain whitespace or control characters")]
    WhitespaceOrControl,
    #[error("browser URL is invalid")]
    Invalid(#[source] url::ParseError),
    #[error("browser URL must use HTTP or HTTPS")]
    UnsupportedScheme,
    #[error("browser URL must include a host")]
    MissingHost,
    #[error("browser URL must not include credentials")]
    Credentials,
}

/// Normalizes a browser URL, adding an HTTPS scheme when one is omitted.
///
/// # Errors
///
/// Returns an error when the input is empty, too long, contains whitespace or
/// control characters, cannot be parsed, uses an unsupported scheme, includes
/// credentials, or lacks a host.
pub fn normalize_browser_url(input: &str) -> Result<String, BrowserUrlError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(BrowserUrlError::Empty);
    }
    if raw.len() > MAX_BROWSER_URL_LEN {
        return Err(BrowserUrlError::TooLong);
    }
    if raw.chars().any(char::is_control) || raw.chars().any(char::is_whitespace) {
        return Err(BrowserUrlError::WhitespaceOrControl);
    }
    if raw == DEFAULT_BROWSER_URL {
        return Ok(raw.to_owned());
    }
    let has_scheme = raw.split_once("://").is_some_and(|(scheme, _)| {
        scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || (index > 0 && matches!(byte, b'+' | b'-' | b'.' | b'0'..=b'9'))
        })
    });
    let candidate = if has_scheme {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let parsed = url::Url::parse(&candidate).map_err(BrowserUrlError::Invalid)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(BrowserUrlError::UnsupportedScheme);
    }
    if parsed.host_str().is_none() {
        return Err(BrowserUrlError::MissingHost);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BrowserUrlError::Credentials);
    }
    let normalized = parsed.to_string();
    if normalized.len() > MAX_BROWSER_URL_LEN {
        return Err(BrowserUrlError::TooLong);
    }
    Ok(normalized)
}

/// Normalizes a browser URL or returns [`DEFAULT_BROWSER_URL`] when omitted.
///
/// # Errors
///
/// Returns the same validation errors as [`normalize_browser_url`] when a
/// non-empty URL is provided.
pub fn normalize_browser_url_or_default(input: Option<&str>) -> Result<String, BrowserUrlError> {
    let input = input.unwrap_or(DEFAULT_BROWSER_URL);
    if input.trim().is_empty() {
        return Ok(DEFAULT_BROWSER_URL.to_owned());
    }
    normalize_browser_url(input)
}

/// Validates a workspace working directory for protocol transport.
///
/// # Errors
///
/// Returns an error when the path is empty, relative, too long, or contains a
/// control character.
pub fn validate_workspace_dir(dir: &str) -> Result<(), ValidationError> {
    if dir.is_empty() {
        return Err(ValidationError::EmptyWorkspaceDir);
    }
    if !dir.starts_with('/') {
        return Err(ValidationError::RelativeWorkspaceDir);
    }
    if dir.len() > MAX_WORKSPACE_DIR_BYTES {
        return Err(ValidationError::WorkspaceDirTooLong);
    }
    if dir.chars().any(char::is_control) {
        return Err(ValidationError::WorkspaceDirControlCharacters);
    }
    Ok(())
}

/// Normalizes the single OpenSSH destination accepted from the desktop UI.
///
/// A user may enter a bare `[user@]host` destination or paste the exact command
/// form `ssh [user@]host`. Harness Harlot strips only that known executable token;
/// options, extra commands, shell syntax, and other executables remain outside
/// this boundary. OpenSSH remains responsible for resolving normal config and
/// agent behavior after the normalized destination is validated.
///
/// # Errors
///
/// Returns a user-facing validation message when the input is empty, too long,
/// contains control characters, or is not one of the two accepted forms.
pub fn normalize_ssh_input(input: &str) -> Result<String, ValidationError> {
    let input = input.trim();
    if input.len() > MAX_SSH_INPUT_LEN {
        return Err(ValidationError::SshInputTooLong);
    }
    if input.is_empty() {
        return Err(ValidationError::SshInputRequired);
    }
    if input.chars().any(char::is_control) {
        return Err(ValidationError::SshInputControlCharacters);
    }
    let parts = input.split_ascii_whitespace().collect::<Vec<_>>();
    let destination = match parts.as_slice() {
        [destination] | ["ssh" | "/usr/bin/ssh" | "/bin/ssh", destination] => *destination,
        _ => {
            return Err(ValidationError::SshInputForm);
        }
    };
    validate_ssh_host(destination)?;
    Ok(destination.to_owned())
}

/// Validates the normalized OpenSSH destination sent to the session service.
///
/// Harness Harlot deliberately accepts only a conservative `[user@]host` or SSH
/// config `Host` alias subset. Option prefixes, ports, commands, shell syntax,
/// whitespace, and control characters are not part of this value.
///
/// # Errors
///
/// Returns a user-facing validation message when `host` is empty, too long, or
/// contains anything outside the accepted destination subset.
pub fn validate_ssh_host(host: &str) -> Result<(), ValidationError> {
    if host.is_empty() {
        return Err(ValidationError::SshHostRequired);
    }
    if host.len() > MAX_SSH_HOST_LEN {
        return Err(ValidationError::SshHostTooLong);
    }
    let (user, host) = match host.split_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() && !host.contains('@') => {
            (Some(user), host)
        }
        Some(_) => return Err(ValidationError::SshUserPrefix),
        None => (None, host),
    };
    if let Some(user) = user
        && !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ValidationError::SshUserCharacters);
    }
    let mut bytes = host.bytes();
    let Some(first) = bytes.next() else {
        return Err(ValidationError::SshHostEmpty);
    };
    if !first.is_ascii_alphanumeric() {
        return Err(ValidationError::SshHostStart);
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
        return Err(ValidationError::SshHostCharacters);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_urls_share_one_normalization_contract() {
        assert_eq!(
            normalize_browser_url(" example.com/docs ").unwrap(),
            "https://example.com/docs"
        );
        assert_eq!(
            normalize_browser_url("http://example.com").unwrap(),
            "http://example.com/"
        );
        assert_eq!(
            normalize_browser_url("example.com/a://b").unwrap(),
            "https://example.com/a://b"
        );
        assert_eq!(
            normalize_browser_url(DEFAULT_BROWSER_URL).unwrap(),
            DEFAULT_BROWSER_URL
        );
        assert_eq!(
            normalize_browser_url_or_default(None).unwrap(),
            DEFAULT_BROWSER_URL
        );
        assert_eq!(
            normalize_browser_url_or_default(Some("   ")).unwrap(),
            DEFAULT_BROWSER_URL
        );
        assert!(matches!(
            normalize_browser_url(""),
            Err(BrowserUrlError::Empty)
        ));
        assert!(matches!(
            normalize_browser_url("https://example.com/a b"),
            Err(BrowserUrlError::WhitespaceOrControl)
        ));
        assert!(matches!(
            normalize_browser_url("file:///tmp/example"),
            Err(BrowserUrlError::UnsupportedScheme)
        ));
        assert!(matches!(
            normalize_browser_url("https://"),
            Err(BrowserUrlError::Invalid(_))
        ));
        assert!(matches!(
            normalize_browser_url("https://user@example.com"),
            Err(BrowserUrlError::Credentials)
        ));
        assert!(matches!(
            normalize_browser_url("https://user:secret@example.com"),
            Err(BrowserUrlError::Credentials)
        ));
        assert!(matches!(
            normalize_browser_url(&format!(
                "https://example.com/{}",
                "a".repeat(MAX_BROWSER_URL_LEN)
            )),
            Err(BrowserUrlError::TooLong)
        ));
    }

    #[test]
    fn workspace_directories_require_absolute_non_controlled_paths() {
        assert!(validate_workspace_dir("/srv/project").is_ok());
        assert!(validate_workspace_dir("").is_err());
        assert!(validate_workspace_dir("relative").is_err());
        assert!(validate_workspace_dir("/srv/\nproject").is_err());
        assert!(
            validate_workspace_dir(&format!("/{}", "x".repeat(MAX_WORKSPACE_DIR_BYTES))).is_err()
        );
    }

    #[test]
    fn ssh_host_validation_accepts_conservative_config_aliases() {
        for host in [
            "build",
            "build-01",
            "prod_us",
            "host.example.com",
            "192.0.2.10",
            "admin@build-01",
            "tailscale_user@host.tailnet-name.ts.net",
        ] {
            assert_eq!(validate_ssh_host(host), Ok(()), "host: {host}");
        }
    }

    #[test]
    fn ssh_input_normalizes_bare_destinations_and_exact_system_ssh_commands() {
        for (input, destination) in [
            ("build", "build"),
            (" admin@build-01\n", "admin@build-01"),
            ("ssh prod_us", "prod_us"),
            (
                "/usr/bin/ssh admin@host.example.com",
                "admin@host.example.com",
            ),
            ("/bin/ssh 192.0.2.10", "192.0.2.10"),
        ] {
            assert_eq!(normalize_ssh_input(input), Ok(destination.to_owned()));
        }

        let padded = format!("{}build{}", " ".repeat(MAX_SSH_INPUT_LEN), " ".repeat(16));
        assert_eq!(normalize_ssh_input(&padded), Ok("build".to_owned()));
    }

    #[test]
    fn ssh_input_rejects_options_extra_commands_and_other_executables() {
        for input in [
            "ssh -A build",
            "ssh -p 22 build",
            "ssh build command",
            "tailscale ssh build",
            "env ssh build",
            "ssh build;bad",
            "ssh\nbuild",
        ] {
            assert!(normalize_ssh_input(input).is_err(), "input: {input:?}");
        }
    }

    #[test]
    fn ssh_host_validation_rejects_option_command_and_shell_injection() {
        for host in [
            "",
            "-A",
            "user@@host",
            "user@",
            "@host",
            "user name@host",
            "host:22",
            "host command",
            "host\nProxyCommand=bad",
            "host;bad",
            "*.example.com",
            "café",
        ] {
            assert!(validate_ssh_host(host).is_err(), "host: {host:?}");
        }
    }
}
