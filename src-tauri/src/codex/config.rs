use crate::codex::{
    lease::{not_reparse, CodexLease},
    CodexError, SMOKE_MODEL,
};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

pub const PINNED_CODEX_EXE: &str = "npm/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe";

fn resolve_pinned_codex_path(appdata: Option<&Path>) -> PathBuf {
    let Some(appdata) = appdata.filter(|path| !path.as_os_str().is_empty() && path.is_absolute()) else {
        return PathBuf::new();
    };
    appdata.join(PINNED_CODEX_EXE)
}

#[cfg(test)]
mod pinned_path_tests {
    use super::*;

    #[test]
    fn pinned_path_is_resolved_beneath_absolute_appdata() {
        #[cfg(windows)]
        let appdata = Path::new(r"C:\Users\USERPROFILE\AppData\Roaming");
        #[cfg(not(windows))]
        let appdata = Path::new("/Users/USERPROFILE/AppData/Roaming");
        assert!(appdata.is_absolute());
        assert_eq!(resolve_pinned_codex_path(Some(appdata)), appdata.join(PINNED_CODEX_EXE));
    }

    #[test]
    fn absent_empty_or_relative_appdata_fails_closed() {
        assert_eq!(resolve_pinned_codex_path(None), PathBuf::new());
        assert_eq!(resolve_pinned_codex_path(Some(Path::new(""))), PathBuf::new());
        assert_eq!(resolve_pinned_codex_path(Some(Path::new("relative/appdata"))), PathBuf::new());
    }
}
pub const PINNED_CODEX_BYTES: u64 = 341_270_320;
pub const PINNED_CODEX_SHA256: &str =
    "2692c8c7bb2cf8b0aefe638741fc90236f4b824c59578ce84994f9de58200993";
pub const SMOKE_PROMPT: &str = "Reply with the exact single token ACK and nothing else.";
pub const MAX_STDIN_PROMPT_BYTES: usize = 1024;
/// `codex_text_v2` stdin: framing + carried same-session turns + the 1024-byte prompt.
pub const MAX_CONTEXT_STDIN_BYTES: usize = 8192;

/// Schema-validated `-c key=value` overrides for `codex-cli 0.163.0`.
///
/// Note on effective tool-surface boundaries:
/// - `include_permissions_instructions` is intentionally NOT overridden to `false` so built-in
///   permission and read-only sandbox instructions remain intact.
/// - In `0.163.0` (`tools/spec_plan.rs:1432`), `ApplyPatchHandler` registration depends on model
///   metadata (`apply_patch_tool_type`), not solely `features.apply_patch_freeform=false`.
///   Therefore we do not claim zero-tool advertisement from config flags alone; containment relies
///   on `--sandbox read-only`, `--ask-for-approval never`, the empty disposable workspace, and
///   immediate `ForbiddenActivity` + Job termination on any non-`agent_message`/`reasoning` event.
pub const LOCKDOWN_CONFIG_OVERRIDES: &[(&str, &str)] = &[
    ("approval_policy", "\"never\""),
    ("sandbox_mode", "\"read-only\""),
    ("web_search", "\"disabled\""),
    ("project_doc_max_bytes", "0"),
    ("project_root_markers", "[]"),
    ("notify", "[]"),
    ("check_for_update_on_startup", "false"),
    ("suppress_unstable_features_warning", "true"),
    ("include_environment_context", "false"),
    ("include_apps_instructions", "false"),
    ("include_collaboration_mode_instructions", "false"),
    ("history.persistence", "\"none\""),
    ("agents.enabled", "false"),
    ("plugins._default.enabled", "false"),
    ("apps._default.enabled", "false"),
    ("analytics.enabled", "false"),
    ("feedback.enabled", "false"),
    ("otel.exporter", "\"none\""),
    ("otel.trace_exporter", "\"none\""),
    ("otel.metrics_exporter", "\"none\""),
    ("tools.experimental_request_user_input.enabled", "false"),
    ("tools.update_plan.enabled", "false"),
    ("features.shell_tool", "false"),
    ("features.unified_exec", "false"),
    ("features.apply_patch_freeform", "false"),
    ("features.apps", "false"),
    ("features.plugins", "false"),
    ("features.hooks", "false"),
    ("features.js_repl", "false"),
    ("features.multi_agent", "false"),
    ("features.multi_agent_v2", "false"),
    ("features.skip_host_skill_discovery", "true"),
    ("features.memories", "false"),
    ("features.view_image", "false"),
    ("features.image_generation", "false"),
    ("features.search_tool", "false"),
    ("features.browser_use", "false"),
    ("features.computer_use", "false"),
    ("features.exec_permission_approvals", "false"),
    ("features.request_permissions_tool", "false"),
    ("features.request_rule", "false"),
    ("features.standalone_web_search", "false"),
    ("features.daemon_auto_start", "false"),
];

const ALLOWLISTED_ENV_KEYS: &[&str] = &[
    "APPDATA",
    "CODEX_HOME",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "PATH",
    "PATHEXT",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERNAME",
    "USERPROFILE",
    "WINDIR",
];

#[derive(Clone, PartialEq, Eq)]
pub struct ExecutablePin {
    pub path: PathBuf,
    pub expected_bytes: u64,
    pub expected_sha256_hex: String,
}

/// Holds an open `FILE_SHARE_READ` handle across `CreateProcessW` so no writer
/// can modify, replace, or delete the verified binary before the kernel maps it.
pub struct VerifiedExecutable {
    canonical_path: PathBuf,
    _read_lock: File,
}

impl VerifiedExecutable {
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }
}

impl ExecutablePin {
    pub fn new(path: PathBuf, expected_bytes: u64, expected_sha256_hex: impl Into<String>) -> Self {
        Self {
            path,
            expected_bytes,
            expected_sha256_hex: expected_sha256_hex.into(),
        }
    }

    pub fn pinned_codex() -> Self {
        Self {
            path: resolve_pinned_codex_path(std::env::var_os("APPDATA").map(PathBuf::from).as_deref()),
            expected_bytes: PINNED_CODEX_BYTES,
            expected_sha256_hex: PINNED_CODEX_SHA256.to_owned(),
        }
    }

    pub fn verify_and_lock(&self) -> Result<VerifiedExecutable, CodexError> {
        if self.expected_bytes == 0
            || self.expected_sha256_hex.len() != 64
            || !self
                .expected_sha256_hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CodexError::InvalidExecutablePin);
        }
        if !has_exe_extension(&self.path) {
            return Err(CodexError::InvalidExecutablePin);
        }
        not_reparse(&self.path).map_err(|_| CodexError::InvalidExecutablePin)?;

        let mut file = open_read_locked(&self.path)?;
        let metadata = file
            .metadata()
            .map_err(|_| CodexError::InvalidExecutablePin)?;
        if !metadata.is_file() || metadata.len() != self.expected_bytes {
            return Err(CodexError::InvalidExecutablePin);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(CodexError::InvalidExecutablePin);
            }
        }
        let canonical_path =
            fs::canonicalize(&self.path).map_err(|_| CodexError::InvalidExecutablePin)?;
        not_reparse(&canonical_path).map_err(|_| CodexError::InvalidExecutablePin)?;
        if !has_exe_extension(&canonical_path) {
            return Err(CodexError::InvalidExecutablePin);
        }

        let mut hasher = Sha256::new();
        let mut buf = [0u8; 65_536];
        let mut total_read = 0u64;
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|_| CodexError::InvalidExecutablePin)?;
            if n == 0 {
                break;
            }
            total_read = total_read
                .checked_add(n as u64)
                .ok_or(CodexError::InvalidExecutablePin)?;
            if total_read > self.expected_bytes {
                return Err(CodexError::InvalidExecutablePin);
            }
            hasher.update(&buf[..n]);
        }
        if total_read != self.expected_bytes {
            return Err(CodexError::InvalidExecutablePin);
        }
        let digest = hex_lower(&hasher.finalize());
        if digest != self.expected_sha256_hex {
            return Err(CodexError::InvalidExecutablePin);
        }
        Ok(VerifiedExecutable {
            canonical_path,
            _read_lock: file,
        })
    }
}

fn has_exe_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(windows)]
fn open_read_locked(path: &Path) -> Result<File, CodexError> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| CodexError::InvalidExecutablePin)
}

#[cfg(not(windows))]
fn open_read_locked(path: &Path) -> Result<File, CodexError> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| CodexError::InvalidExecutablePin)
}

/// Validated invocation specification for a single non-interactive turn.
pub struct CodexCommandSpec {
    pin: ExecutablePin,
    workspace_dir: PathBuf,
    args: Vec<OsString>,
    stdin_prompt: Vec<u8>,
    env_vars: Vec<(OsString, OsString)>,
}

impl CodexCommandSpec {
    pub fn pinned_ack_smoke(lease: &CodexLease) -> Result<Self, CodexError> {
        let root = lease.validate()?;
        let workspace_dir = root.join("workspace");
        not_reparse(&workspace_dir).map_err(|_| CodexError::InvalidConfig)?;
        let args = build_pinned_ack_args(&workspace_dir)?;
        let env_vars = scrubbed_host_environment()?;
        Ok(Self {
            pin: ExecutablePin::pinned_codex(),
            workspace_dir,
            args,
            stdin_prompt: SMOKE_PROMPT.as_bytes().to_vec(),
            env_vars,
        })
    }

    /// Production `codex_text_v1` turn: the pinned executable, the same locked-down
    /// non-interactive argument set as the reviewed ACK smoke (read-only sandbox,
    /// approval never, ephemeral, ignore user config, fixed model, stdin prompt),
    /// and the user's prompt on stdin. 1..=1024 UTF-8 bytes; no caller-chosen
    /// executable, cwd, args, env, model or flags.
    pub fn codex_text_v1(lease: &CodexLease, prompt: &str) -> Result<Self, CodexError> {
        if prompt.is_empty() || prompt.len() > MAX_STDIN_PROMPT_BYTES {
            return Err(CodexError::InvalidConfig);
        }
        let mut spec = Self::pinned_ack_smoke(lease)?;
        spec.stdin_prompt = prompt.as_bytes().to_vec();
        Ok(spec)
    }

    /// Production `codex_text_v2` turn: exactly the `codex_text_v1` lockdown, but the stdin
    /// text may also carry earlier turns of the same Workbench session (<= 8192 UTF-8 bytes,
    /// composed by the app; Codex itself keeps no memory because the run stays ephemeral).
    pub fn codex_text_v2(lease: &CodexLease, text: &str) -> Result<Self, CodexError> {
        if text.is_empty() || text.len() > MAX_CONTEXT_STDIN_BYTES {
            return Err(CodexError::InvalidConfig);
        }
        let mut spec = Self::pinned_ack_smoke(lease)?;
        spec.stdin_prompt = text.as_bytes().to_vec();
        Ok(spec)
    }

    pub fn custom_for_verified_child(
        pin: ExecutablePin,
        lease: &CodexLease,
        args: Vec<OsString>,
        stdin_prompt: Vec<u8>,
        extra_env: Vec<(OsString, OsString)>,
    ) -> Result<Self, CodexError> {
        let root = lease.validate()?;
        let workspace_dir = root.join("workspace");
        not_reparse(&workspace_dir).map_err(|_| CodexError::InvalidConfig)?;
        if stdin_prompt.len() > MAX_CONTEXT_STDIN_BYTES {
            return Err(CodexError::InvalidConfig);
        }
        for arg in &args {
            reject_nul(arg)?;
        }
        let mut env_vars = scrubbed_host_environment()?;
        for (k, v) in extra_env {
            if k.is_empty() || contains_eq(&k) {
                return Err(CodexError::InvalidConfig);
            }
            reject_nul(&k)?;
            reject_nul(&v)?;
            env_vars.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&k));
            env_vars.push((k, v));
        }
        Ok(Self {
            pin,
            workspace_dir,
            args,
            stdin_prompt,
            env_vars,
        })
    }

    pub fn pin(&self) -> &ExecutablePin {
        &self.pin
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    pub fn stdin_prompt(&self) -> &[u8] {
        &self.stdin_prompt
    }

    pub fn env_vars(&self) -> &[(OsString, OsString)] {
        &self.env_vars
    }
}

pub fn build_pinned_ack_args(workspace_dir: &Path) -> Result<Vec<OsString>, CodexError> {
    reject_nul(workspace_dir.as_os_str())?;
    let mut args = vec![
        OsString::from("--ask-for-approval"),
        OsString::from("never"),
        OsString::from("--no-daemon"),
        OsString::from("exec"),
        OsString::from("--json"),
        OsString::from("--ephemeral"),
        OsString::from("--ignore-user-config"),
        OsString::from("--skip-git-repo-check"),
        OsString::from("--strict-config"),
        OsString::from("--color"),
        OsString::from("never"),
        OsString::from("--sandbox"),
        OsString::from("read-only"),
        OsString::from("--model"),
        OsString::from(SMOKE_MODEL),
        OsString::from("-C"),
        workspace_dir.as_os_str().to_os_string(),
    ];
    for &(key, value) in LOCKDOWN_CONFIG_OVERRIDES {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("{key}={value}")));
    }
    args.push(OsString::from("-"));
    Ok(args)
}

fn scrubbed_host_environment() -> Result<Vec<(OsString, OsString)>, CodexError> {
    let mut out = Vec::with_capacity(ALLOWLISTED_ENV_KEYS.len());
    for &key in ALLOWLISTED_ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() && validate_os_no_nul(&value) {
                out.push((OsString::from(key), value));
            }
        }
    }
    Ok(out)
}

fn contains_eq(s: &OsStr) -> bool {
    s.to_string_lossy().contains('=')
}

fn validate_os_no_nul(s: &OsStr) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        !s.encode_wide().any(|u| u == 0)
    }
    #[cfg(not(windows))]
    {
        !s.to_string_lossy().contains('\0')
    }
}

pub(crate) fn reject_nul(s: &OsStr) -> Result<(), CodexError> {
    if validate_os_no_nul(s) {
        Ok(())
    } else {
        Err(CodexError::InvalidConfig)
    }
}

#[cfg(windows)]
pub(crate) fn encode_windows_command_line(
    program: &Path,
    args: &[OsString],
) -> Result<Vec<u16>, CodexError> {
    let mut out = Vec::new();
    append_quoted_wide(&mut out, program.as_os_str())?;
    for arg in args {
        out.push(b' ' as u16);
        append_quoted_wide(&mut out, arg)?;
    }
    out.push(0);
    Ok(out)
}

#[cfg(windows)]
fn append_quoted_wide(out: &mut Vec<u16>, arg: &OsStr) -> Result<(), CodexError> {
    use std::os::windows::ffi::OsStrExt;
    reject_nul(arg)?;
    let wide: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = wide.is_empty()
        || wide
            .iter()
            .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16);
    if !needs_quotes {
        out.extend_from_slice(&wide);
        return Ok(());
    }
    out.push(b'"' as u16);
    let mut backslashes = 0usize;
    for c in wide {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else if c == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            out.push(b'"' as u16);
            backslashes = 0;
        } else {
            if backslashes > 0 {
                out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
                backslashes = 0;
            }
            out.push(c);
        }
    }
    if backslashes > 0 {
        out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    }
    out.push(b'"' as u16);
    Ok(())
}

#[cfg(windows)]
pub(crate) fn encode_windows_environment_block(
    vars: &[(OsString, OsString)],
) -> Result<Vec<u16>, CodexError> {
    use std::os::windows::ffi::OsStrExt;
    let mut entries: Vec<(String, Vec<u16>)> = Vec::with_capacity(vars.len());
    for (k, v) in vars {
        if k.is_empty() || contains_eq(k) {
            return Err(CodexError::InvalidConfig);
        }
        reject_nul(k)?;
        reject_nul(v)?;
        let sort_key = k.to_string_lossy().to_ascii_uppercase();
        let mut line: Vec<u16> = k.encode_wide().collect();
        line.push(b'=' as u16);
        line.extend(v.encode_wide());
        line.push(0);
        entries.push((sort_key, line));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block = Vec::new();
    for (_, line) in entries {
        block.extend_from_slice(&line);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn windows_command_line_quotes_spaces_backslashes_and_toml_strings() {
        let cmd = encode_windows_command_line(
            Path::new(r"C:\Program Files\Codex\codex.exe"),
            &[
                OsString::from("-c"),
                OsString::from("approval_policy=\"never\""),
                OsString::from(r"C:\My Temp\trail\"),
                OsString::from(""),
            ],
        )
        .unwrap();
        assert_eq!(*cmd.last().unwrap(), 0);
        let decoded = String::from_utf16(&cmd[..cmd.len() - 1]).unwrap();
        assert_eq!(
            decoded,
            r#""C:\Program Files\Codex\codex.exe" -c "approval_policy=\"never\"" "C:\My Temp\trail\\" """#
        );
        assert_eq!(
            encode_windows_command_line(Path::new("codex.exe"), &[OsString::from("bad\0arg")]),
            Err(CodexError::InvalidConfig)
        );
    }

    #[test]
    fn windows_environment_block_is_sorted_double_nul_terminated_and_rejects_invalid_keys() {
        let block = encode_windows_environment_block(&[
            (OsString::from("TEMP"), OsString::from(r"C:\Temp")),
            (OsString::from("APPDATA"), OsString::from(r"C:\App")),
        ])
        .unwrap();
        let expected: Vec<u16> = "APPDATA=C:\\App\0TEMP=C:\\Temp\0\0"
            .encode_utf16()
            .collect();
        assert_eq!(block, expected);
        assert_eq!(
            encode_windows_environment_block(&[(OsString::from("BAD=KEY"), OsString::from("1"))]),
            Err(CodexError::InvalidConfig)
        );
    }
}
