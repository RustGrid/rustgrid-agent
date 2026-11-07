//! `rustgrid-agent trace install | status | uninstall`: sets up Claude Code so
//! every session on this computer is traced, and explains what is set up.
//!
//! By default the hooks go into Claude Code's managed settings, which users
//! cannot override; writing them needs administrator rights, so a normal user
//! run asks `sudo` for that one step. `--user` writes the person's own
//! settings instead, which suits trying tracing out but can be removed.
//! Existing settings are merged, never replaced: only hooks that run
//! `rustgrid-agent trace hook` are added or removed.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::{api::RustGridClient, config::AppContext};

/// Each hook Claude Code runs, its tool matcher, and its timeout in seconds.
const HOOKS: [(&str, Option<&str>, u64); 5] = [
    ("SessionStart", None, 10),
    ("UserPromptSubmit", None, 15),
    ("PostToolUse", Some("*"), 5),
    ("Stop", None, 5),
    ("SessionEnd", None, 10),
];
/// Marks a hook command as ours, whatever path the binary lives at.
const HOOK_MARKER: &str = " trace hook ";
const LINK_COMMAND: &str = include_str!("../../packaging/claude-code/commands/rustgrid-link.md");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Claude Code managed settings: applies to everyone, cannot be overridden.
    Managed,
    /// The person's own `~/.claude/settings.json`.
    User,
}

#[derive(Debug)]
pub struct InstallOptions {
    pub scope: Scope,
    pub login: bool,
    pub open_browser: bool,
    pub instance: Option<String>,
}

pub fn managed_settings_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    } else {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn user_settings_path() -> Result<PathBuf> {
    Ok(home()?.join(".claude").join("settings.json"))
}

fn link_command_path() -> Result<PathBuf> {
    Ok(home()?
        .join(".claude")
        .join("commands")
        .join("rustgrid-link.md"))
}

/// Puts the current `/rustgrid-link` command in the person's Claude Code
/// commands when it is missing or out of date. Hooks call this, so installs
/// done as root by device management, and upgrades, give everyone the command.
pub fn ensure_link_command() -> Result<()> {
    let path = link_command_path()?;
    if fs::read_to_string(&path).ok().as_deref() != Some(LINK_COMMAND) {
        fs::create_dir_all(path.parent().expect("command path has a parent"))?;
        fs::write(&path, LINK_COMMAND)?;
    }
    Ok(())
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The path hooks should run. A Homebrew install resolves into a versioned
/// Cellar directory that the next upgrade deletes, so use the stable
/// `<prefix>/bin` link instead.
pub fn stable_binary_path(executable: &Path) -> PathBuf {
    let text = executable.to_string_lossy();
    if let Some((prefix, _)) = text.split_once("/Cellar/rustgrid-agent/") {
        return Path::new(prefix).join("bin").join("rustgrid-agent");
    }
    executable.to_path_buf()
}

fn current_binary() -> Result<PathBuf> {
    Ok(stable_binary_path(
        &std::env::current_exe().context("could not find the rustgrid-agent executable")?,
    ))
}

fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/._-+".contains(character))
    {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

fn is_our_hook(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(HOOK_MARKER))
}

/// Removes our hooks from Claude Code settings and drops anything left empty.
/// Returns whether anything was removed.
pub fn remove_hooks(settings: &mut Value) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut removed = false;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = entries.len();
                entries.retain(|hook| !is_our_hook(hook));
                removed |= entries.len() != before;
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|entries| !entries.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|groups| !groups.is_empty()));
    if hooks.is_empty()
        && let Some(object) = settings.as_object_mut()
    {
        object.remove("hooks");
    }
    removed
}

/// Adds our hooks for `binary`, replacing any earlier ones and keeping every
/// other setting and hook.
pub fn merge_hooks(settings: &mut Value, binary: &Path) {
    if !settings.is_object() {
        *settings = Value::Object(Map::new());
    }
    remove_hooks(settings);
    let object = settings.as_object_mut().expect("settings are an object");
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let hooks = hooks.as_object_mut().expect("hooks are an object");
    for (event, matcher, timeout) in HOOKS {
        let mut group = json!({
            "hooks": [{
                "type": "command",
                "command": format!("{} trace hook {event}", shell_quote(binary)),
                "timeout": timeout
            }]
        });
        if let Some(matcher) = matcher {
            group["matcher"] = json!(matcher);
        }
        let groups = hooks.entry(event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        groups
            .as_array_mut()
            .expect("hook groups are an array")
            .push(group);
    }
}

/// The command our installed hooks run, if they are all present.
pub fn installed_hook_command(settings: &Value) -> Option<String> {
    let hooks = settings.get("hooks")?.as_object()?;
    let mut command = None;
    for (event, _, _) in HOOKS {
        let found = hooks
            .get(event)?
            .as_array()?
            .iter()
            .filter_map(|group| group.get("hooks").and_then(Value::as_array))
            .flatten()
            .filter(|hook| is_our_hook(hook))
            .filter_map(|hook| hook.get("command").and_then(Value::as_str))
            .next()?;
        command = found
            .strip_suffix(&format!("{HOOK_MARKER}{event}"))
            .map(str::to_owned);
        command.as_ref()?;
    }
    command
}

fn read_settings(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON; fix it before installing",
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

fn write_settings(path: &Path, settings: &Value, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let temporary = path.with_extension("json.rustgrid-tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(mode)
        .open(&temporary)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    file.write_all(serde_json::to_string_pretty(settings)?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

/// Writes the managed hooks; must run as root. Used directly by MDM and,
/// through `sudo`, by a person's `trace install`.
pub fn install_managed(binary: &Path) -> Result<()> {
    if !is_root() {
        bail!("writing Claude Code managed settings needs administrator rights; run with sudo");
    }
    let path = managed_settings_path();
    let mut settings = read_settings(&path)?;
    merge_hooks(&mut settings, binary);
    write_settings(&path, &settings, 0o644)?;
    println!("Installed RustGrid hooks in {}", path.display());
    Ok(())
}

fn uninstall_managed() -> Result<()> {
    if !is_root() {
        bail!("changing Claude Code managed settings needs administrator rights; run with sudo");
    }
    let path = managed_settings_path();
    let mut settings = read_settings(&path)?;
    if remove_hooks(&mut settings) {
        write_settings(&path, &settings, 0o644)?;
        println!("Removed RustGrid hooks from {}", path.display());
    }
    Ok(())
}

/// Runs this binary's `trace` subcommand with sudo for the one step that
/// needs administrator rights.
fn sudo_trace(binary: &Path, args: &[&str]) -> Result<()> {
    println!(
        "Claude Code managed settings are shared by everyone on this computer, so sudo asks for your password."
    );
    let status = Command::new("sudo")
        .arg(binary)
        .arg("trace")
        .args(args)
        .status()
        .context("could not run sudo")?;
    if !status.success() {
        bail!("sudo did not complete; nothing was changed in managed settings");
    }
    Ok(())
}

pub fn install(config_path: &Path, options: InstallOptions) -> Result<()> {
    let binary = current_binary()?;
    match options.scope {
        Scope::Managed if is_root() => install_managed(&binary)?,
        Scope::Managed => sudo_trace(
            &binary,
            &[
                "install",
                "--system-only",
                "--binary",
                &binary.to_string_lossy(),
            ],
        )?,
        Scope::User => {
            let path = user_settings_path()?;
            let mut settings = read_settings(&path)?;
            merge_hooks(&mut settings, &binary);
            write_settings(&path, &settings, 0o600)?;
            println!("Installed RustGrid hooks in {}", path.display());
        }
    }
    if is_root() {
        println!(
            "Each person on this computer signs in once with `rustgrid-agent login`; Claude Code asks them to before their first prompt."
        );
        return Ok(());
    }

    let command = link_command_path()?;
    fs::create_dir_all(command.parent().expect("command path has a parent"))?;
    fs::write(&command, LINK_COMMAND)?;

    if options.login && !signed_in(config_path) {
        println!("\nSigning this computer in to RustGrid…");
        let mut context = AppContext::load_for_login(config_path, options.instance.as_deref())?;
        crate::auth::login(&mut context, options.open_browser)?;
    }
    println!();
    status(config_path)
}

pub fn uninstall(scope: Option<Scope>) -> Result<()> {
    let binary = current_binary()?;
    if matches!(scope, None | Some(Scope::User)) && !is_root() {
        let path = user_settings_path()?;
        let mut settings = read_settings(&path)?;
        if remove_hooks(&mut settings) {
            write_settings(&path, &settings, 0o600)?;
            println!("Removed RustGrid hooks from {}", path.display());
        }
        let _ = fs::remove_file(link_command_path()?);
    }
    if matches!(scope, None | Some(Scope::Managed)) {
        let managed = read_settings(&managed_settings_path())?;
        if managed.to_string().contains(HOOK_MARKER) {
            if is_root() {
                uninstall_managed()?;
            } else {
                sudo_trace(&binary, &["uninstall", "--system-only"])?;
            }
        }
    }
    println!("Claude Code sessions on this computer are no longer traced.");
    Ok(())
}

pub fn uninstall_system_only() -> Result<()> {
    uninstall_managed()
}

fn signed_in(config_path: &Path) -> bool {
    AppContext::load(config_path)
        .ok()
        .is_some_and(|context| context.api_key.is_some() && context.worker_id.is_some())
}

fn mark(ok: bool) -> &'static str {
    if ok { "✓" } else { "✗" }
}

fn ago(time: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(time)
        .unwrap_or_default()
        .as_secs();
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

/// Explains whether this computer's Claude Code sessions are traced, and
/// what to do when they are not. Returns an error when tracing is not working.
pub fn status(config_path: &Path) -> Result<()> {
    let binary = current_binary()?;
    let expected = shell_quote(&binary);
    let mut problems = Vec::new();
    println!(
        "rustgrid-agent {} at {}",
        env!("CARGO_PKG_VERSION"),
        binary.display()
    );

    let managed_path = managed_settings_path();
    let managed = read_settings(&managed_path)
        .ok()
        .and_then(|settings| installed_hook_command(&settings));
    let user = user_settings_path()
        .ok()
        .and_then(|path| read_settings(&path).ok());
    let user_hooks = user.as_ref().and_then(installed_hook_command);
    match (&managed, &user_hooks) {
        (Some(command), _) => {
            println!(
                "{} Hooks: managed settings ({}); cannot be turned off by users",
                mark(true),
                managed_path.display()
            );
            if *command != expected {
                println!(
                    "  ! They run {command}, not this binary; run `rustgrid-agent trace install` to update them."
                );
            }
        }
        (None, Some(command)) => {
            println!(
                "{} Hooks: your own settings (~/.claude/settings.json); you can remove them",
                mark(true)
            );
            if *command != expected {
                println!(
                    "  ! They run {command}, not this binary; run `rustgrid-agent trace install --user` to update them."
                );
            }
        }
        (None, None) => {
            println!("{} Hooks: not installed", mark(false));
            problems.push("run `rustgrid-agent trace install`");
        }
    }
    if user
        .as_ref()
        .and_then(|settings| settings.get("disableAllHooks"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        println!(
            "  ! ~/.claude/settings.json sets disableAllHooks; managed hooks should still run, your own hooks will not."
        );
    }

    match AppContext::load(config_path) {
        Ok(context) if context.api_key.is_some() && context.worker_id.is_some() => {
            let worker_id = context.worker_id.clone().unwrap_or_default();
            let reachable = RustGridClient::with_timeout(&context, Duration::from_secs(5))
                .and_then(|client| client.worker_status(&worker_id));
            match reachable {
                Ok(_) => println!(
                    "{} Signed in to {} as worker {worker_id}",
                    mark(true),
                    context.instance_url
                ),
                Err(error) => {
                    println!(
                        "{} Signed in, but RustGrid did not answer: {error:#}",
                        mark(false)
                    );
                    println!(
                        "  Sessions are kept on this computer and uploaded once RustGrid answers."
                    );
                }
            }
        }
        _ => {
            println!("{} Not signed in", mark(false));
            problems.push("run `rustgrid-agent login`");
        }
    }

    if let Ok(cwd) = std::env::current_dir()
        && let Some(branch) = Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|branch| branch.trim().to_owned())
    {
        match super::ticket_keys_in_branch(&branch).first() {
            Some(key) => println!("  This repository's branch {branch} links sessions to {key}."),
            None => println!(
                "  Branch {branch} names no ticket: link sessions with /rustgrid-link ADD-123."
            ),
        }
    }

    if let Some(latest) = latest_session() {
        println!("  Last traced session: {latest}");
    }

    if problems.is_empty() {
        println!("\nClaude Code sessions on this computer are traced.");
        Ok(())
    } else {
        bail!("tracing is not set up: {}", problems.join(", then "))
    }
}

fn latest_session() -> Option<String> {
    let dir = home().ok()?.join(".rustgrid").join("trace");
    let (modified, path) = fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max_by_key(|(modified, _)| *modified)?;
    let session: Value = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    let problem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|id| fs::read_to_string(super::problem_path(&dir, id)).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    let ticket = session
        .pointer("/ticket/key")
        .and_then(Value::as_str)
        .unwrap_or("no ticket");
    let pending = session
        .get("transcript_path")
        .and_then(Value::as_str)
        .and_then(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .zip(session.get("offset").and_then(Value::as_u64))
        .map(|(length, offset)| length.saturating_sub(offset))
        .unwrap_or(0);
    let state = if session.get("finalized").and_then(Value::as_bool) == Some(true) {
        "ended"
    } else if pending > 0 {
        "upload pending"
    } else {
        "up to date"
    };
    let mut line = format!("{ticket}, {}, {state}", ago(modified));
    if let Some(problem) = problem {
        line.push_str(&format!("\n  Last upload problem: {problem}"));
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homebrew_installs_use_the_stable_bin_link() {
        assert_eq!(
            stable_binary_path(Path::new(
                "/opt/homebrew/Cellar/rustgrid-agent/1.6.1/bin/rustgrid-agent"
            )),
            PathBuf::from("/opt/homebrew/bin/rustgrid-agent")
        );
        assert_eq!(
            stable_binary_path(Path::new("/Users/ada/.cargo/bin/rustgrid-agent")),
            PathBuf::from("/Users/ada/.cargo/bin/rustgrid-agent")
        );
    }

    #[test]
    fn the_shipped_template_is_what_install_writes() {
        let mut settings = json!({});
        merge_hooks(&mut settings, Path::new("/usr/local/bin/rustgrid-agent"));
        let template: Value = serde_json::from_str(include_str!(
            "../../packaging/claude-code/managed-settings.json"
        ))
        .unwrap();
        assert_eq!(settings, template);
    }

    #[test]
    fn install_keeps_other_settings_and_replaces_only_our_hooks() {
        let mut settings = json!({
            "permissions": {"deny": ["Read(./.env)"]},
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Edit", "hooks": [{"type": "command", "command": "prettier --write"}]},
                    {"matcher": "*", "hooks": [{"type": "command", "command": "/old/rustgrid-agent trace hook PostToolUse"}]}
                ],
                "Notification": [{"hooks": [{"type": "command", "command": "say done"}]}]
            }
        });
        merge_hooks(&mut settings, Path::new("/opt/homebrew/bin/rustgrid-agent"));
        assert_eq!(settings["permissions"], json!({"deny": ["Read(./.env)"]}));
        assert_eq!(
            settings["hooks"]["Notification"][0]["hooks"][0]["command"],
            "say done"
        );
        let post = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 2, "{post:?}");
        assert_eq!(post[0]["hooks"][0]["command"], "prettier --write");
        assert_eq!(
            post[1]["hooks"][0]["command"],
            "/opt/homebrew/bin/rustgrid-agent trace hook PostToolUse"
        );
        assert_eq!(
            installed_hook_command(&settings).as_deref(),
            Some("/opt/homebrew/bin/rustgrid-agent")
        );

        // Installing twice changes nothing.
        let once = settings.clone();
        merge_hooks(&mut settings, Path::new("/opt/homebrew/bin/rustgrid-agent"));
        assert_eq!(settings, once);

        // Uninstalling leaves exactly what was there before, minus our hooks.
        assert!(remove_hooks(&mut settings));
        assert_eq!(
            settings,
            json!({
                "permissions": {"deny": ["Read(./.env)"]},
                "hooks": {
                    "PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "prettier --write"}]}],
                    "Notification": [{"hooks": [{"type": "command", "command": "say done"}]}]
                }
            })
        );
        assert!(installed_hook_command(&settings).is_none());
        assert!(!remove_hooks(&mut settings));
    }

    #[test]
    fn uninstalling_the_only_hooks_removes_the_hooks_key() {
        let mut settings = json!({"model": "opus"});
        merge_hooks(&mut settings, Path::new("/usr/local/bin/rustgrid-agent"));
        remove_hooks(&mut settings);
        assert_eq!(settings, json!({"model": "opus"}));
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        let mut settings = json!({});
        merge_hooks(
            &mut settings,
            Path::new("/Users/Ada Lovelace/bin/rustgrid-agent"),
        );
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/Users/Ada Lovelace/bin/rustgrid-agent' trace hook Stop"
        );
        assert_eq!(
            installed_hook_command(&settings).as_deref(),
            Some("'/Users/Ada Lovelace/bin/rustgrid-agent'")
        );
    }
}
