#![allow(non_snake_case)]
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
mod proc_guard {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    const JOB_INFO_CLASS: u32 = 9;
    const JOB_KILL_ON_CLOSE: u32 = 0x2000;

    #[repr(C)]
    #[derive(Default)]
    struct JobBasicLimitInfo {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_op_count: u64,
        write_op_count: u64,
        other_op_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobExtendedLimitInfo {
        basic: JobBasicLimitInfo,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *const c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(job: *mut c_void, class: u32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    pub struct GuardHandle(*mut c_void);
    impl Drop for GuardHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0); }
            }
        }
    }

    /// Binds the child process to a Windows Job Object so it terminates with the router.
    /// @param child Spawned MCP server process.
    /// @return RAII handle keeping the Job Object alive.
    pub fn AttachChild(child: &Child) -> Option<GuardHandle> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() { return None; }
            let mut info = JobExtendedLimitInfo::default();
            info.basic.limit_flags = JOB_KILL_ON_CLOSE;
            if SetInformationJobObject(job, JOB_INFO_CLASS, &info as *const _ as *const c_void, std::mem::size_of::<JobExtendedLimitInfo>() as u32) == 0 {
                CloseHandle(job);
                return None;
            }
            AssignProcessToJobObject(job, child.as_raw_handle() as *mut c_void);
            Some(GuardHandle(job))
        }
    }
}

struct McpTarget {
    cmd: String,
    args: Vec<String>,
    env: HashMap<String, String>,
}

fn NonEmptyEnv(key: &str) -> Option<String> {
    env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Resolves Ropilot Studio's per-user data directory across Windows, macOS, and Linux.
/// @return Path to the `ropilot` userData directory.
fn UserDataDir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = NonEmptyEnv("APPDATA")
            .or_else(|| NonEmptyEnv("USERPROFILE").map(|h| PathBuf::from(h).join("AppData").join("Roaming").to_string_lossy().to_string()))
            .unwrap_or_default();
        return PathBuf::from(base).join("ropilot");
    }
    #[cfg(target_os = "macos")]
    {
        let home = NonEmptyEnv("HOME").unwrap_or_default();
        return PathBuf::from(home).join("Library").join("Application Support").join("ropilot");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let cfg = NonEmptyEnv("XDG_CONFIG_HOME")
            .unwrap_or_else(|| PathBuf::from(NonEmptyEnv("HOME").unwrap_or_default()).join(".config").to_string_lossy().to_string());
        return PathBuf::from(cfg).join("ropilot");
    }
}

fn ParseHex4(slice: &str) -> Option<u16> {
    let bytes = slice.as_bytes();
    if bytes.len() < 4 || !bytes[..4].iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(&slice[..4], 16).ok()
}

/// Decodes a JSON string literal starting at its opening quote, including UTF-16 surrogates.
/// @param slice Slice beginning with `"`.
/// @return Decoded string and consumed byte length, or `None` if malformed.
fn ParseString(slice: &str) -> Option<(String, usize)> {
    if !slice.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut i = 1usize;
    let bytes = slice.as_bytes();
    while i < bytes.len() {
        let ch = slice[i..].chars().next()?;
        if ch == '"' {
            return Some((out, i + 1));
        }
        if ch != '\\' {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        i += 1;
        let esc = slice[i..].chars().next()?;
        i += esc.len_utf8();
        match esc {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                if let Some(hi) = ParseHex4(&slice[i..]) {
                    i += 4;
                    if (0xD800..=0xDBFF).contains(&hi) && slice[i..].starts_with("\\u") {
                        if let Some(lo) = ParseHex4(&slice[i + 2..]) {
                            if (0xDC00..=0xDFFF).contains(&lo) {
                                i += 6;
                                let cp = 0x10000 + (((hi as u32 - 0xD800) << 10) | (lo as u32 - 0xDC00));
                                if let Some(c) = char::from_u32(cp) { out.push(c); }
                                continue;
                            }
                        }
                    }
                    if let Some(c) = char::from_u32(hi as u32) {
                        out.push(c);
                    }
                } else {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    None
}

/// Extracts a balanced `{...}` or `[...]` block starting at index 0.
/// @param text JSON slice beginning with `open`.
/// @param open Opening delimiter.
/// @param close Closing delimiter.
/// @return Subslice covering the balanced block.
fn ExtractBlock(text: &str, open: char, close: char) -> Option<&str> {
    if !text.starts_with(open) {
        return None;
    }
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (idx, ch) in text.char_indices() {
        if escaped { escaped = false; continue; }
        if ch == '\\' && quoted { escaped = true; continue; }
        if ch == '"' { quoted = !quoted; continue; }
        if quoted { continue; }
        if ch == open { depth += 1; }
        else if ch == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(&text[..=idx]);
            }
        }
    }
    None
}

/// Finds a top-level field value (`depth == 1`) inside a JSON object.
/// @param obj Enclosing JSON object slice.
/// @param key Exact field name to locate.
/// @return Value slice immediately after `:`.
fn FindField<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let bytes = obj.as_bytes();
    let mut pos = 0usize;
    let mut depth = 0usize;
    while pos < bytes.len() {
        match bytes[pos] {
            b'{' | b'[' => { depth += 1; pos += 1; }
            b'}' | b']' => { depth = depth.saturating_sub(1); pos += 1; }
            b'"' => {
                let Some((name, len)) = ParseString(&obj[pos..]) else {
                    return None;
                };
                if depth == 1 && name == key {
                    if let Some(val) = obj[pos + len..].trim_start().strip_prefix(':') {
                        return Some(val.trim_start());
                    }
                }
                pos += len;
            }
            _ => pos += 1,
        }
    }
    None
}

fn ReadStringArray(obj: &str, key: &str) -> Vec<String> {
    let Some(val) = FindField(obj, key) else { return Vec::new(); };
    let Some(arr) = ExtractBlock(val, '[', ']') else { return Vec::new(); };
    let mut items = Vec::new();
    let mut pos = 1usize;
    while pos < arr.len() {
        let rest = &arr[pos..];
        let Some(q) = rest.find('"') else { break; };
        let Some((s, len)) = ParseString(&rest[q..]) else { break; };
        items.push(s);
        pos += q + len;
    }
    items
}

fn SkipJsonValue(slice: &str) -> usize {
    if slice.starts_with('"') {
        return ParseString(slice).map(|(_, len)| len).unwrap_or(slice.len());
    }
    if slice.starts_with('{') {
        return ExtractBlock(slice, '{', '}').map(str::len).unwrap_or(slice.len());
    }
    if slice.starts_with('[') {
        return ExtractBlock(slice, '[', ']').map(str::len).unwrap_or(slice.len());
    }
    slice.find(|c: char| c == ',' || c == '}').unwrap_or(slice.len())
}

fn ReadEnvMap(obj: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(val) = FindField(obj, "env") else { return map; };
    let Some(block) = ExtractBlock(val, '{', '}') else { return map; };
    let mut pos = 1usize;
    while pos < block.len() {
        let rest = &block[pos..];
        let Some(kOff) = rest.find('"') else { break; };
        let Some((key, kLen)) = ParseString(&rest[kOff..]) else { break; };
        let Some(after) = rest[kOff + kLen..].trim_start().strip_prefix(':') else { break; };
        let trimmed = after.trim_start();
        let valOff = trimmed.as_ptr() as usize - rest.as_ptr() as usize;
        if let Some((v, vLen)) = ParseString(trimmed) {
            map.insert(key, v);
            pos += valOff + vLen;
        } else {
            pos += valOff + SkipJsonValue(trimmed);
        }
    }
    map
}

fn ResolveConfigPath() -> Option<String> {
    if let Some(p) = NonEmptyEnv("ROPILOT_SESSION_MCP_CONFIG") {
        if Path::new(&p).is_file() {
            return Some(p);
        }
    }
    let marker = fs::read_to_string(UserDataDir().join("agy-session-mcp.txt")).ok()?;
    let s = marker.trim().to_string();
    if !s.is_empty() && Path::new(&s).is_file() { Some(s) } else { None }
}

fn ParseSessionConfig(raw: &str) -> Option<McpTarget> {
    let servers = ExtractBlock(FindField(raw, "mcpServers")?, '{', '}')?;
    let ropilot = ExtractBlock(FindField(servers, "ropilot")?, '{', '}')?;
    let (cmd, _) = ParseString(FindField(ropilot, "command")?)?;
    Some(McpTarget {
        cmd,
        args: ReadStringArray(ropilot, "args"),
        env: ReadEnvMap(ropilot),
    })
}

/// Loads the live `ropilot` MCP server command, args, and env for this session.
/// @return Parsed `McpTarget` when a valid session config is active.
fn LoadSessionTarget() -> Option<McpTarget> {
    let cfgPath = ResolveConfigPath()?;
    let raw = fs::read_to_string(cfgPath).ok()?;
    ParseSessionConfig(&raw)
}

fn DefaultTarget() -> McpTarget {
    let dataDir = UserDataDir();
    let binName = if cfg!(windows) { "ropilot-infra-helper.exe" } else { "ropilot-infra-helper" };
    let mut env = HashMap::new();
    env.insert("ROPILOT_USERDATA".to_string(), dataDir.to_string_lossy().to_string());
    McpTarget {
        cmd: dataDir.join("bin").join(binName).to_string_lossy().to_string(),
        args: vec!["mcp".to_string(), "--ropilot-external-mcp".to_string()],
        env,
    }
}

fn ExecTarget(target: McpTarget) -> i32 {
    let mut cmd = Command::new(&target.cmd);
    cmd.args(&target.args).envs(&target.env).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("ropilot-mcp-router failed to exec {}: {}", target.cmd, err);
        return 1;
    }

    #[cfg(windows)]
    {
        match cmd.spawn() {
            Ok(mut child) => {
                let _guard = proc_guard::AttachChild(&child);
                child.wait().ok().and_then(|s| s.code()).unwrap_or(1)
            }
            Err(err) => {
                eprintln!("ropilot-mcp-router failed to spawn {}: {}", target.cmd, err);
                1
            }
        }
    }
}

fn main() {
    let target = LoadSessionTarget().unwrap_or_else(DefaultTarget);
    std::process::exit(ExecTarget(target));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ParsesSurrogatesAndEscapes() {
        let (s, len) = ParseString(r#""hi \uD83D\uDE00 \b\f""#).unwrap();
        assert_eq!(s, "hi \u{1F600} \u{0008}\u{000C}");
        assert_eq!(len, 22);
        assert!(ParseString(r#""\u12""#).is_none());
    }

    #[test]
    fn RespectsDepthAndNullValues() {
        let raw = r#"{"other":{"ropilot":{"command":"wrong"}},"mcpServers":{"ropilot":{"command":"right","args":["a"],"env":{"NEST":{"ropilot":"x"},"OK":"1"}}}}"#;
        let target = ParseSessionConfig(raw).unwrap();
        assert_eq!(target.cmd, "right");
        assert_eq!(target.args, vec!["a"]);
        assert_eq!(target.env.get("OK").map(String::as_str), Some("1"));
        assert!(!target.env.contains_key("ropilot"));

        let nullCfg = r#"{"mcpServers":null,"fallback":{"ropilot":{"command":"bad"}}}"#;
        assert!(ParseSessionConfig(nullCfg).is_none());
    }
}