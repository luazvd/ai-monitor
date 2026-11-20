//! Every signed-in session a provider has on this machine, not only its
//! default one.
//!
//! Claude Code and Codex keep one login per config directory: `~/.claude`
//! (or `CLAUDE_CONFIG_DIR`) and `~/.codex` (or `CODEX_HOME`). The Codex
//! desktop app runs on the CLI's own `CODEX_HOME`, so it shares that login.
//! Antigravity keeps one login per product: `agy` and the desktop app each
//! save their own token file under `~/.gemini`. Any two of these may be the
//! same Google/OpenAI/Anthropic account or different ones.
//!
//! The default directory is what each vendor's plain tab already reads. This
//! module finds the *others*: a config directory set in a running process's
//! environment (a terminal started with `CODEX_HOME=~/.codex-work codex`), and
//! Antigravity's per-product token files. A directory only shows up in a live
//! environment while its process runs, so every one seen is remembered in the
//! cache (`sessions.json`) until its credential file disappears.
//!
//! Identity is read offline wherever the session names it (`.claude.json`,
//! Codex's `id_token`, `agy`'s `id_token`). Nothing here touches the network.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::vendor::VendorId;

const REMEMBERED_FILE: &str = "sessions.json";
const CLAUDE_ENV: &str = "CLAUDE_CONFIG_DIR";
const CODEX_ENV: &str = "CODEX_HOME";
const CLAUDE_CREDENTIALS: &str = ".credentials.json";
const CODEX_AUTH: &str = "auth.json";

/// A session beyond the vendor's default one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub vendor: VendorId,
    /// Account label for the synthetic tab and its cache subdir. Stable for a
    /// given directory or product, so its cached usage survives restarts.
    pub label: String,
    /// The credential file: `.credentials.json`, `auth.json`, or a token file.
    pub path: PathBuf,
    /// The account's email, when the session names it offline.
    pub identity: Option<String>,
}

/// Config directories seen in running processes, by vendor slug.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remembered {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dirs: BTreeMap<String, BTreeSet<PathBuf>>,
}

/// Every extra session on this machine, remembering newly seen directories.
/// Best-effort: an unreadable home, cache, or `/proc` just yields fewer.
pub fn discover(config: &Config) -> Vec<Session> {
    let Ok(home) = crate::cache::home_dir() else {
        return Vec::new();
    };
    let remembered_path = crate::cache::xdg_cache_dir()
        .ok()
        .map(|dir| dir.join("ai-monitor").join(REMEMBERED_FILE));
    let mut remembered = remembered_path
        .as_deref()
        .map(load_remembered)
        .unwrap_or_default();
    let before = remembered.clone();
    for (vendor, dir) in env_dirs_from_processes() {
        remembered
            .dirs
            .entry(vendor.slug().to_string())
            .or_default()
            .insert(dir);
    }
    let sessions = discover_with(config, &home, &mut remembered);
    if remembered != before
        && let Some(path) = remembered_path
    {
        let _ = save_remembered(&path, &remembered);
    }
    sessions
}

/// Test seam for [`discover`]: `home` and the remembered directories are
/// injected. Directories whose credential file is gone are dropped from
/// `remembered`, so a deleted login stops being polled.
pub fn discover_with(config: &Config, home: &Path, remembered: &mut Remembered) -> Vec<Session> {
    let mut sessions = Vec::new();
    for (vendor, file) in [
        (VendorId::Anthropic, CLAUDE_CREDENTIALS),
        (VendorId::Openai, CODEX_AUTH),
    ] {
        let known = known_dirs(config, home, vendor);
        let Some(dirs) = remembered.dirs.get_mut(vendor.slug()) else {
            continue;
        };
        dirs.retain(|dir| dir.join(file).is_file());
        for dir in dirs.iter() {
            if known.contains(&canonical(dir)) {
                continue;
            }
            let path = dir.join(file);
            sessions.push(Session {
                vendor,
                label: dir_label(dir),
                identity: dir_identity(vendor, dir, &path),
                path,
            });
        }
    }
    remembered.dirs.retain(|_, dirs| !dirs.is_empty());

    let configured: BTreeSet<PathBuf> = config
        .antigravity
        .accounts
        .iter()
        .map(|a| canonical(&a.token_path))
        .collect();
    for (label, rel) in [
        ("agy", crate::antigravity::credential::CLI_TOKEN_FILE),
        (
            "desktop",
            crate::antigravity::credential::DESKTOP_TOKEN_FILE,
        ),
    ] {
        let path = home.join(rel);
        if !path.is_file() || configured.contains(&canonical(&path)) {
            continue;
        }
        let identity = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| crate::antigravity::credential::parse_keyring_blob(&raw).ok())
            .and_then(|token| token.email);
        sessions.push(Session {
            vendor: VendorId::Antigravity,
            label: label.to_string(),
            path,
            identity,
        });
    }
    sessions
}

/// Directories the vendor's own tabs already read: the default one and every
/// configured account. A discovered session never duplicates them.
fn known_dirs(config: &Config, home: &Path, vendor: VendorId) -> BTreeSet<PathBuf> {
    let parent = |p: &Path| p.parent().map(canonical);
    let mut out = BTreeSet::new();
    match vendor {
        VendorId::Anthropic => {
            out.insert(canonical(&home.join(".claude")));
            out.extend(
                config
                    .anthropic
                    .credentials_path
                    .as_deref()
                    .and_then(parent),
            );
            out.extend(
                config
                    .anthropic
                    .all_accounts()
                    .iter()
                    .map(|a| canonical(&a.config_dir())),
            );
        }
        VendorId::Openai => {
            out.insert(canonical(&home.join(".codex")));
            out.extend(config.openai.codex_auth_path.as_deref().and_then(parent));
            out.extend(
                config
                    .openai
                    .accounts
                    .iter()
                    .filter_map(|a| parent(&a.codex_auth_path)),
            );
        }
        _ => {}
    }
    out
}

fn dir_identity(vendor: VendorId, dir: &Path, credential: &Path) -> Option<String> {
    match vendor {
        VendorId::Anthropic => crate::anthropic::cli_account::account_email_in(
            &crate::anthropic::cli_account::marker_path(dir),
        ),
        VendorId::Openai => crate::openai::creds::read_from(credential)
            .ok()?
            .tokens
            .email_from_id_token(),
        _ => None,
    }
}

/// A cache-safe label that stays the same for the same directory: its name,
/// plus a short hash so `~/a/.codex` and `~/b/.codex` never share a cache.
pub fn dir_label(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let name: String = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-');
    let digest = Sha256::digest(canonical(dir).to_string_lossy().as_bytes());
    let hash: String = digest.iter().take(3).map(|b| format!("{b:02x}")).collect();
    if name.is_empty() {
        format!("session-{hash}")
    } else {
        format!("{name}-{hash}")
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn load_remembered(path: &Path) -> Remembered {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_remembered(path: &Path, remembered: &Remembered) -> crate::error::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::error::AppError::io_at(dir, e))?;
    }
    let bytes = serde_json::to_vec_pretty(remembered)?;
    crate::cache::atomic_write(path, &bytes)
}

/// `CLAUDE_CONFIG_DIR` / `CODEX_HOME` values from this user's running
/// processes. Only absolute paths count: a relative one means nothing outside
/// the process's own working directory.
#[cfg(target_os = "linux")]
fn env_dirs_from_processes() -> Vec<(VendorId, PathBuf)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // Another user's environ is unreadable; that is the expected case.
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        for found in parse_environ(&environ) {
            if !out.contains(&found) {
                out.push(found);
            }
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn env_dirs_from_processes() -> Vec<(VendorId, PathBuf)> {
    Vec::new()
}

/// The config-dir variables in one NUL-separated `environ` block.
pub fn parse_environ(environ: &[u8]) -> Vec<(VendorId, PathBuf)> {
    environ
        .split(|b| *b == 0)
        .filter_map(|var| {
            let var = std::str::from_utf8(var).ok()?;
            let (key, value) = var.split_once('=')?;
            let vendor = match key {
                CLAUDE_ENV => VendorId::Anthropic,
                CODEX_ENV => VendorId::Openai,
                _ => return None,
            };
            let path = PathBuf::from(value.trim_end_matches('/'));
            path.is_absolute().then_some((vendor, path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_token(claims: &str) -> String {
        use base64::Engine as _;
        format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims)
        )
    }

    fn codex_login(dir: &Path, email: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let auth = serde_json::json!({
            "tokens": {
                "access_token": "at",
                "refresh_token": "rt",
                "id_token": id_token(&format!(r#"{{"email":"{email}"}}"#)),
            }
        });
        std::fs::write(dir.join(CODEX_AUTH), auth.to_string()).unwrap();
    }

    fn claude_login(dir: &Path, email: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(CLAUDE_CREDENTIALS), "{}").unwrap();
        std::fs::write(
            dir.join(".claude.json"),
            format!(r#"{{"oauthAccount":{{"emailAddress":"{email}"}}}}"#),
        )
        .unwrap();
    }

    fn remember(dirs: &[(VendorId, &Path)]) -> Remembered {
        let mut r = Remembered::default();
        for (vendor, dir) in dirs {
            r.dirs
                .entry(vendor.slug().to_string())
                .or_default()
                .insert(dir.to_path_buf());
        }
        r
    }

    #[test]
    fn environ_yields_only_absolute_config_dirs() {
        let block = b"PATH=/bin\0CODEX_HOME=/home/u/.codex-work/\0CLAUDE_CONFIG_DIR=rel\0CLAUDE_CONFIG_DIR=/home/u/.claude-b\0";
        assert_eq!(
            parse_environ(block),
            vec![
                (VendorId::Openai, PathBuf::from("/home/u/.codex-work")),
                (VendorId::Anthropic, PathBuf::from("/home/u/.claude-b")),
            ]
        );
    }

    #[test]
    fn extra_dirs_become_sessions_with_their_identity() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        let codex = home.join(".codex-work");
        let claude = home.join(".claude-b");
        codex_login(&codex, "work@example.test");
        claude_login(&claude, "b@example.test");
        let mut remembered =
            remember(&[(VendorId::Openai, &codex), (VendorId::Anthropic, &claude)]);

        let sessions = discover_with(&Config::default(), home, &mut remembered);

        let find = |v: VendorId| sessions.iter().find(|s| s.vendor == v).unwrap();
        assert_eq!(
            find(VendorId::Openai).identity.as_deref(),
            Some("work@example.test")
        );
        assert_eq!(find(VendorId::Openai).path, codex.join(CODEX_AUTH));
        assert_eq!(
            find(VendorId::Anthropic).identity.as_deref(),
            Some("b@example.test")
        );
        assert!(find(VendorId::Openai).label.starts_with("codex-work-"));
    }

    #[test]
    fn the_default_dirs_are_never_extra_sessions() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        codex_login(&home.join(".codex"), "me@example.test");
        claude_login(&home.join(".claude"), "me@example.test");
        let mut remembered = remember(&[
            (VendorId::Openai, &home.join(".codex")),
            (VendorId::Anthropic, &home.join(".claude")),
        ]);
        assert!(discover_with(&Config::default(), home, &mut remembered).is_empty());
    }

    #[test]
    fn a_dir_whose_login_is_gone_is_forgotten() {
        let td = tempfile::tempdir().unwrap();
        let gone = td.path().join(".codex-old");
        std::fs::create_dir_all(&gone).unwrap();
        let mut remembered = remember(&[(VendorId::Openai, &gone)]);
        assert!(discover_with(&Config::default(), td.path(), &mut remembered).is_empty());
        assert!(remembered.dirs.is_empty());
    }

    #[test]
    fn antigravity_token_files_are_sessions_per_product() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        let cli = home.join(crate::antigravity::credential::CLI_TOKEN_FILE);
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(
            &cli,
            format!(
                r#"{{"id_token":"{}","token":{{"access_token":"at"}}}}"#,
                id_token(r#"{"email":"cli@example.test"}"#)
            ),
        )
        .unwrap();
        std::fs::write(
            home.join(crate::antigravity::credential::DESKTOP_TOKEN_FILE),
            r#"{"token":{"access_token":"at2"}}"#,
        )
        .unwrap();

        let sessions = discover_with(&Config::default(), home, &mut Remembered::default());

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].label, "agy");
        assert_eq!(sessions[0].identity.as_deref(), Some("cli@example.test"));
        assert_eq!(sessions[1].label, "desktop");
        assert_eq!(sessions[1].identity, None);
    }

    #[test]
    fn dir_labels_are_cache_safe_and_distinct() {
        let a = dir_label(Path::new("/x/a/.codex"));
        let b = dir_label(Path::new("/x/b/.codex"));
        assert_ne!(a, b);
        assert!(a.starts_with("codex-"));
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }
}
