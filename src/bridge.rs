#![allow(non_snake_case)]
use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::str::Chars;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

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

    /// Binds the child process to a Windows Job Object so it terminates with the parent.
    /// @param child Spawned `agy` child process.
    /// @return RAII handle keeping the Job Object alive.
    pub fn AttachChild(child: &Child) -> Option<GuardHandle> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
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

#[cfg(not(windows))]
mod proc_guard {
    use std::process::Child;

    pub struct GuardHandle;
    pub fn AttachChild(_child: &Child) -> Option<GuardHandle> {
        None
    }
}

struct BridgeCfg {
    sessionId: String,
    sysPromptFile: String,
    model: String,
    effort: String,
    mcpConfig: String,
    printPrompt: Option<String>,
    isVersionCheck: bool,
    isStreamJson: bool,
}

struct RouteCfg {
    provider: String,
    model: String,
    effort: String,
}

struct TurnState {
    sessionId: String,
    modelName: String,
    convId: String,
    savedConvId: String,
    initEmitted: bool,
    inFlight: bool,
    textBuf: String,
    emittedText: bool,
    suppressedSteps: HashSet<u64>,
    emittedImages: HashSet<String>,
    stderrTail: Vec<String>,
    transcriptLock: Arc<Mutex<Option<PathBuf>>>,
    stdoutLock: Arc<Mutex<()>>,
}

enum BridgeEvent {
    StdinLine(String),
    StdinClosed,
    ChildExited(u64, i32),
}

/// Resolves Ropilot Studio's per-user data directory across Windows, macOS, and Linux.
/// @return Path to the `ropilot` userData directory.
fn DataDir() -> PathBuf {
    #[cfg(windows)]
    {
        return PathBuf::from(env::var("APPDATA").unwrap_or_default()).join("ropilot");
    }
    #[cfg(target_os = "macos")]
    {
        return HomeDir().join("Library").join("Application Support").join("ropilot");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let cfg = env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| HomeDir().join(".config").to_string_lossy().to_string());
        return PathBuf::from(cfg).join("ropilot");
    }
}

/// Resolves the current user's home directory across Windows and POSIX platforms.
/// @return Path to the user's home directory.
fn HomeDir() -> PathBuf {
    #[cfg(windows)]
    {
        return PathBuf::from(env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default());
    }
    #[cfg(not(windows))]
    {
        return PathBuf::from(env::var("HOME").unwrap_or_default());
    }
}

/// Returns current UNIX epoch time in milliseconds.
/// @return Milliseconds since 1970-01-01T00:00:00Z.
fn NowMs() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn DecodeEscape(code: char, iter: &mut Peekable<Chars>) -> Option<char> {
    match code {
        '"' => Some('"'),
        '\\' => Some('\\'),
        '/' => Some('/'),
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'u' => {
            let mut hex = String::with_capacity(4);
            for _ in 0..4 {
                hex.push(iter.next()?);
            }
            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
        }
        other => Some(other),
    }
}

fn ParseJsonString(slice: &str) -> Option<(String, usize)> {
    let mut iter = slice.chars().peekable();
    if iter.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    let mut bytes = 1usize;
    while let Some(ch) = iter.next() {
        bytes += ch.len_utf8();
        if ch == '"' {
            return Some((out, bytes));
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(esc) = iter.next() else { break; };
        bytes += esc.len_utf8();
        if esc == 'u' {
            let mut hex = String::with_capacity(4);
            for _ in 0..4 {
                if let Some(h) = iter.next() {
                    bytes += h.len_utf8();
                    hex.push(h);
                }
            }
            if let Some(cp) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(cp);
            }
        } else if let Some(chDec) = DecodeEscape(esc, &mut iter) {
            out.push(chDec);
        }
    }
    None
}

fn FindFieldValue<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let bytes = json.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        if bytes[pos] != b'"' {
            pos += 1;
            continue;
        }
        let Some((name, len)) = ParseJsonString(&json[pos..]) else {
            pos += 1;
            continue;
        };
        if name == key {
            if let Some(val) = json[pos + len..].trim_start().strip_prefix(':') {
                return Some(val.trim_start());
            }
        }
        pos += len;
    }
    None
}

fn ExtractStr(json: &str, key: &str) -> Option<String> {
    let val = FindFieldValue(json, key)?;
    if !val.starts_with('"') {
        return None;
    }
    ParseJsonString(val).map(|(s, _)| s)
}

fn ExtractSpan(text: &str, open: char, close: char) -> Option<(usize, usize)> {
    let start = text.find(open)?;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (idx, ch) in text[start..].char_indices() {
        if escaped { escaped = false; continue; }
        if ch == '\\' && quoted { escaped = true; continue; }
        if ch == '"' { quoted = !quoted; continue; }
        if quoted { continue; }
        if ch == open { depth += 1; }
        else if ch == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some((start, start + idx + ch.len_utf8()));
            }
        }
    }
    None
}

fn ExtractObj(json: &str, key: &str) -> Option<String> {
    let val = FindFieldValue(json, key)?;
    if !val.starts_with('{') {
        return None;
    }
    let (s, e) = ExtractSpan(val, '{', '}')?;
    Some(val[s..e].to_string())
}

fn ExtractStrOrObj(json: &str, key: &str) -> Option<String> {
    let val = FindFieldValue(json, key)?;
    if val.starts_with('{') {
        let (s, e) = ExtractSpan(val, '{', '}')?;
        return Some(val[s..e].to_string());
    }
    if val.starts_with('"') {
        let (decoded, _) = ParseJsonString(val)?;
        let trimmed = decoded.trim();
        if trimmed.starts_with('{') && trimmed.ends_with('}') {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn ExtractNum(json: &str, key: &str) -> u64 {
    let Some(val) = FindFieldValue(json, key) else { return 0; };
    let digits: String = val.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().unwrap_or(0)
}

fn Unquote(raw: &str) -> String {
    let s = raw.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn EscapeJson(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn DecodeBase64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for b in input.bytes() {
        if b.is_ascii_whitespace() || b == b'=' { continue; }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Parses Claude CLI flags passed by `ropilot-infra` into a structured config.
/// @param args Process `argv` slice.
/// @return Parsed `BridgeCfg`.
fn ParseCfg(args: &[String]) -> BridgeCfg {
    let mut cfg = BridgeCfg {
        sessionId: String::new(),
        sysPromptFile: String::new(),
        model: String::new(),
        effort: String::new(),
        mcpConfig: String::new(),
        printPrompt: None,
        isVersionCheck: false,
        isStreamJson: false,
    };

    for (i, tok) in args.iter().enumerate() {
        if tok == "--version" || tok == "-v" {
            cfg.isVersionCheck = true;
            continue;
        }
        if tok.contains("stream-json") {
            cfg.isStreamJson = true;
            continue;
        }
        let (flag, valOpt) = if let Some((k, v)) = tok.split_once('=') {
            (k, Some(v))
        } else {
            (tok.as_str(), args.get(i + 1).map(String::as_str))
        };
        let Some(val) = valOpt else { continue; };
        match flag {
            "--session-id" | "--resume" => cfg.sessionId = val.to_string(),
            "--system-prompt-file" => cfg.sysPromptFile = val.to_string(),
            "--model" => cfg.model = val.to_string(),
            "--effort" => cfg.effort = val.to_string(),
            "--mcp-config" => cfg.mcpConfig = val.to_string(),
            "-p" | "--print" => cfg.printPrompt = Some(val.to_string()),
            _ => {}
        }
    }
    cfg
}

fn ChatIdFromMcp(mcpPath: &str) -> Option<String> {
    let name = Path::new(mcpPath).file_name()?.to_str()?;
    let stem = name.strip_suffix(".json")?.strip_prefix("ropilot-mcp-config-")?;
    let trimmed = if let Some(rest) = stem.strip_prefix('p') {
        match rest.split_once('-') {
            Some((digits, tail)) if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) => tail,
            _ => stem,
        }
    } else {
        stem
    };
    let idx = trimmed.rfind('-')?;
    let id = &trimmed[..idx];
    if id.is_empty() { None } else { Some(id.to_string()) }
}

fn ReadRouteFile(path: &Path) -> Option<RouteCfg> {
    let raw = fs::read_to_string(path).ok()?;
    let provider = ExtractStr(&raw, "provider").unwrap_or_default();
    let model = ExtractStr(&raw, "model").unwrap_or_default();
    let effort = ExtractStr(&raw, "effort").unwrap_or_default();
    if provider.is_empty() && model.is_empty() {
        return None;
    }
    Some(RouteCfg { provider, model, effort })
}

/// Resolves the UI-selected provider, model, and effort for the active chat.
/// @param cfg Parsed CLI config.
/// @return Route override when written by Ropilot Studio.
fn ResolveRoute(cfg: &BridgeCfg) -> Option<RouteCfg> {
    let dir = DataDir();
    if let Some(chatId) = ChatIdFromMcp(&cfg.mcpConfig) {
        let safe: String = chatId.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
        if let Some(route) = ReadRouteFile(&dir.join("agy-routes").join(format!("{}.json", safe))) {
            return Some(route);
        }
    }
    if !cfg.mcpConfig.trim().is_empty() && !cfg.model.trim().starts_with("agy:") {
        return ReadRouteFile(&dir.join("agy-route-latest.json"));
    }
    None
}

fn IsClaudeName(model: &str) -> bool {
    let low = model.trim().to_ascii_lowercase();
    low.starts_with("claude-") || low.starts_with("opus") || low.starts_with("sonnet") || low.starts_with("haiku") || low.starts_with("fable")
}

/// Locates the `agy` CLI binary across Windows, macOS, and Linux.
/// @return Resolved path or binary name on `PATH`.
fn ResolveAgyBin() -> PathBuf {
    let binName = if cfg!(windows) { "agy.exe" } else { "agy" };
    let candidates = [
        HomeDir().join(".gemini").join("bin").join(binName),
        PathBuf::from("/opt/homebrew/bin").join(binName),
        PathBuf::from("/usr/local/bin").join(binName),
    ];
    for cand in candidates {
        if cand.is_file() {
            return cand;
        }
    }
    PathBuf::from(binName)
}

/// Queries and caches the live model IDs reported by `agy models`.
/// @return Static slice of model IDs supported by the installed `agy` binary.
fn LiveAgyModels() -> &'static [String] {
    static MODELS: OnceLock<Vec<String>> = OnceLock::new();
    MODELS.get_or_init(|| {
        let Ok(out) = Command::new(ResolveAgyBin()).arg("models").output() else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .filter_map(|l| l.split('\t').next())
            .map(str::trim)
            .filter(|id| !id.is_empty() && !id.starts_with("Fetching"))
            .map(str::to_string)
            .collect()
    })
}

fn StripEffortSuffix(model: &str) -> &str {
    for sfx in ["-low", "-medium", "-high", "-xhigh"] {
        if let Some(base) = model.strip_suffix(sfx) {
            return base;
        }
    }
    model
}

/// Matches a UI model ID and reasoning effort to an exact live `agy` model ID.
/// @param rawModel Base or suffixed model ID from Ropilot UI.
/// @param rawEffort Effort tier (`low`, `medium`, `high`, `xhigh`, `max`).
/// @return Concrete model ID accepted by `agy`.
fn ResolveAgyModel(rawModel: &str, rawEffort: &str) -> String {
    let stripped = rawModel.trim().strip_prefix("agy:").unwrap_or(rawModel.trim());
    let models = LiveAgyModels();
    let effort = rawEffort.trim().to_ascii_lowercase();
    let base = if effort.is_empty() { stripped } else { StripEffortSuffix(stripped) };

    if !base.is_empty() && !IsClaudeName(base) {
        if !effort.is_empty() {
            let candidate = format!("{}-{}", base, if effort == "max" { "xhigh" } else { effort.as_str() });
            if models.iter().any(|m| m == &candidate) {
                return candidate;
            }
            if effort == "max" || effort == "xhigh" {
                let highFallback = format!("{}-high", base);
                if models.iter().any(|m| m == &highFallback) {
                    return highFallback;
                }
            }
        }
        if models.is_empty() || models.iter().any(|m| m == base) {
            return base.to_string();
        }
        if let Some(matched) = models.iter().find(|m| StripEffortSuffix(m) == base) {
            return matched.clone();
        }
    }

    models.iter()
        .find(|m| m.starts_with("gemini-"))
        .or_else(|| models.first())
        .cloned()
        .unwrap_or_else(|| base.to_string())
}

/// Finds the original `claude-real` binary when forwarding non-Gemini sessions.
/// @return Path to original Claude Code CLI if installed.
fn FindRealClaude() -> Option<PathBuf> {
    let realName = if cfg!(windows) { "claude-real.exe" } else { "claude-real" };
    let managed = DataDir().join("claude").join(realName);
    if managed.is_file() {
        return Some(managed);
    }
    let selfExe = env::current_exe().ok().and_then(|p| fs::canonicalize(p).ok());
    let candidates = [
        HomeDir().join(".local").join("bin").join(if cfg!(windows) { "claude.exe" } else { "claude" }),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ];
    for cand in candidates {
        if !cand.is_file() { continue; }
        if selfExe.is_some() && fs::canonicalize(&cand).ok() == selfExe { continue; }
        return Some(cand);
    }
    None
}

fn ForwardRealClaude(args: &[String]) -> Option<i32> {
    let real = FindRealClaude()?;
    let status = Command::new(real)
        .args(&args[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .ok()?;
    Some(status.code().unwrap_or(1))
}

fn IsAuthProbe(prompt: &str) -> bool {
    let low = prompt.trim().to_ascii_lowercase();
    low.is_empty() || low == "say hi" || low.contains("reply with just ok") || low.contains("reply with ok")
}

fn PatchMcpConfig(cfgPath: &Path, routerPath: &Path, dataDir: &Path) {
    let Ok(raw) = fs::read_to_string(cfgPath) else { return; };
    if !raw.contains("ropilot-infra-helper") {
        return;
    }
    let Some(sVal) = FindFieldValue(&raw, "mcpServers") else { return; };
    let sOff = sVal.as_ptr() as usize - raw.as_ptr() as usize;
    let Some((s0, s1)) = ExtractSpan(&raw[sOff..], '{', '}') else { return; };
    let serversSlice = &raw[sOff + s0..sOff + s1];

    let Some(rVal) = FindFieldValue(serversSlice, "ropilot") else { return; };
    let rOff = rVal.as_ptr() as usize - raw.as_ptr() as usize;
    let Some((r0, r1)) = ExtractSpan(&raw[rOff..], '{', '}') else { return; };
    if !raw[rOff + r0..rOff + r1].contains("ropilot-infra-helper") {
        return;
    }

    let repl = format!(
        "{{\"command\":\"{}\",\"args\":[],\"env\":{{\"ROPILOT_USERDATA\":\"{}\"}}}}",
        EscapeJson(&routerPath.to_string_lossy()),
        EscapeJson(&dataDir.to_string_lossy())
    );
    let updated = format!("{}{}{}", &raw[..rOff + r0], repl, &raw[rOff + r1..]);
    let _ = fs::write(cfgPath, updated);
}

fn EnsureMcpRouter() {
    let dataDir = DataDir();
    let routerName = if cfg!(windows) { "ropilot-mcp-router.exe" } else { "ropilot-mcp-router" };
    let router = dataDir.join("bin").join(routerName);
    if !router.is_file() {
        return;
    }
    let gemDir = HomeDir().join(".gemini");
    PatchMcpConfig(&gemDir.join("antigravity").join("mcp_config.json"), &router, &dataDir);
    PatchMcpConfig(&gemDir.join("config").join("mcp_config.json"), &router, &dataDir);
}

fn WriteSessionMarker(mcpConfig: &str) {
    let s = mcpConfig.trim();
    if s.is_empty() { return; }
    let _ = fs::write(DataDir().join("agy-session-mcp.txt"), s);
}

fn ClearSessionMarker(mcpConfig: &str) {
    let marker = DataDir().join("agy-session-mcp.txt");
    if let Ok(cur) = fs::read_to_string(&marker) {
        if cur.trim() == mcpConfig.trim() {
            let _ = fs::remove_file(marker);
        }
    }
}

fn ResolveTranscript(sessionId: &str) -> Option<PathBuf> {
    if sessionId.trim().is_empty() { return None; }
    let cwd = env::current_dir().ok()?.to_string_lossy().to_string();
    let encoded: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let dir = HomeDir().join(".claude").join("projects").join(encoded);
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{}.jsonl", sessionId.trim())))
}

fn AppendTranscript(pathOpt: &Option<PathBuf>, line: &str) {
    let Some(path) = pathOpt else { return; };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", line);
    }
}

fn LookupConvId(sessionId: &str) -> Option<String> {
    let target = sessionId.trim();
    if target.is_empty() { return None; }
    let raw = fs::read_to_string(DataDir().join("session_map.txt")).ok()?;
    raw.lines()
        .filter_map(|l| l.split_once('='))
        .rev()
        .find(|(k, v)| k.trim() == target && !v.trim().is_empty())
        .map(|(_, v)| v.trim().to_string())
}

fn SaveConvId(sessionId: &str, convId: &str) {
    if sessionId.trim().is_empty() || convId.trim().is_empty() { return; }
    let path = DataDir().join("session_map.txt");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}={}", sessionId.trim(), convId.trim());
    }
}

fn EmitStdout(payload: &str, lock: &Arc<Mutex<()>>) {
    let _guard = lock.lock();
    let mut out = io::stdout();
    let _ = out.write_all(payload.as_bytes());
    let _ = out.flush();
}

fn EnsureInit(state: &mut TurnState) {
    if state.initEmitted { return; }
    let evt = format!("{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{}\"}}\n", EscapeJson(&state.sessionId));
    EmitStdout(&evt, &state.stdoutLock);
    state.initEmitted = true;
}

fn EmitAssistantText(text: &str, state: &mut TurnState) {
    if text.is_empty() { return; }
    EnsureInit(state);
    let escaped = EscapeJson(text);
    let evt = format!("{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n", escaped);
    EmitStdout(&evt, &state.stdoutLock);
    if let Ok(guard) = state.transcriptLock.lock() {
        let rec = format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"{}\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}",
            EscapeJson(&state.sessionId), escaped
        );
        AppendTranscript(&*guard, &rec);
    }
}

fn StageImage(srcPath: &str) -> String {
    let trimmed = srcPath.trim().trim_start_matches("file:///");
    let clean = if cfg!(windows) { trimmed.replace('/', "\\") } else { trimmed.to_string() };
    let src = Path::new(&clean);
    if !src.is_file() { return clean; }
    let tmp = env::temp_dir();
    if clean.to_ascii_lowercase().starts_with(&tmp.to_string_lossy().to_ascii_lowercase()) {
        return clean;
    }
    let dir = tmp.join("ropilot-agy-images");
    if fs::create_dir_all(&dir).is_err() { return clean; }
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png");
    let dest = dir.join(format!("studio_{}.{}", NowMs(), ext));
    if fs::copy(src, &dest).is_ok() { dest.to_string_lossy().to_string() } else { clean }
}

fn EmitChatImage(imgPath: &str, stepNum: u64, state: &mut TurnState) {
    let trimmed = imgPath.trim().trim_start_matches("file:///");
    let key = if cfg!(windows) { trimmed.replace('/', "\\") } else { trimmed.to_string() };
    if key.is_empty() || !state.emittedImages.insert(key.clone()) { return; }
    EnsureInit(state);
    let cmd = format!("__ROPILOT_CHAT_IMAGE__:{}", StageImage(&key));
    let id = format!("img_{}", stepNum);
    let useEvt = format!(
        "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"{}\",\"name\":\"Bash\",\"input\":{{\"command\":\"{}\"}}}}]}}}}\n",
        id, EscapeJson(&cmd)
    );
    EmitStdout(&useEvt, &state.stdoutLock);
    let resEvt = format!(
        "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"{}\",\"content\":\"ok\",\"is_error\":false}}]}}}}\n",
        id
    );
    EmitStdout(&resEvt, &state.stdoutLock);
}

fn ExtractOffloadedImage(text: &str) -> Option<String> {
    let idx = text.find("file:///")?;
    let rest = &text[idx + 8..];
    let end = rest.find(|c: char| c == ']' || c.is_whitespace() || c == '"').unwrap_or(rest.len());
    let cand = &rest[..end];
    if IsImagePath(cand) { Some(cand.to_string()) } else { None }
}

fn IsImagePath(path: &str) -> bool {
    let low = path.trim().to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp"].iter().any(|e| low.ends_with(e))
}

fn CleanDisplayPath(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() { return "workspace".to_string(); }
    let low = s.replace('\\', "/").to_ascii_lowercase();
    if low.contains("appdata/local/temp")
        || low.starts_with("/tmp/")
        || low.starts_with("/var/folders/")
        || low.contains("antigravity-cli/brain/")
        || low.contains("/.system_generated/steps/")
    {
        return Path::new(s).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    }
    s.to_string()
}

fn IsNativeRopilotTool(name: &str) -> bool {
    matches!(
        name,
        "ropilot_edit_ctx_run_command" | "ropilot_getproperties" | "ropilot_getuserselection"
            | "ropilot_is_playing" | "ropilot_listchildren" | "ropilot_resetdata"
            | "ropilot_search" | "ropilot_searchbyclass" | "ropilot_should_playtest"
            | "ropilot_studio_connection" | "ropilot_test_run_client_lua" | "ropilot_test_run_server_lua"
            | "ropilot_test_start_playtest" | "ropilot_test_stop_playtest" | "search_asset" | "insert_asset"
            | "ropilot_blender_run_python" | "ropilot_blender_import" | "ropilot_blender_screenshot"
            | "ropilot_blender_import_rig" | "ropilot_blender_import_animation" | "ropilot_blender_import_model"
            | "ropilot_blender_render" | "ropilot_blender_upload_images" | "ropilot_reference_sheet"
            | "ropilot_generate_image" | "ropilot_generate_ui_art" | "ropilot_generate_vfx_texture"
            | "ropilot_studio_screenshot" | "ropilot_vfx_capture" | "ropilot_vfx_textures"
            | "ropilot_video_info" | "ropilot_video_frames"
            | "ropilot_sourcecontrol_commitscript" | "ropilot_sourcecontrol_createscript"
            | "ropilot_sourcecontrol_disablescript" | "ropilot_sourcecontrol_grepscripts"
            | "ropilot_sourcecontrol_movescript" | "ropilot_sourcecontrol_previewgrep"
            | "ropilot_sourcecontrol_requestscript"
    )
}

fn MapMcpPayload(line: &str) -> (String, String) {
    let params = ExtractObj(line, "parameters").unwrap_or_else(|| line.to_string());
    let server = Unquote(&ExtractStr(&params, "ServerName").unwrap_or_default());
    let inner = Unquote(&ExtractStr(&params, "ToolName").unwrap_or_else(|| "mcp_tool".to_string()));
    let args = ExtractStrOrObj(&params, "Arguments").unwrap_or_else(|| "{}".to_string());

    if server == "ropilot" && IsNativeRopilotTool(&inner) {
        return (format!("mcp__ropilot__{}", inner), args);
    }

    let target = ExtractStr(&args, "Path")
        .or_else(|| ExtractStr(&args, "ParentPath"))
        .or_else(|| ExtractStr(&args, "Query"))
        .or_else(|| ExtractStr(&args, "path"))
        .unwrap_or_default();
    let summary = if target.is_empty() { format!("{}:{}", server, inner) } else { format!("{}:{} {}", server, inner, target) };
    ("Bash".to_string(), format!("{{\"command\":\"{}\"}}", EscapeJson(&summary)))
}

fn MapToolPayload(line: &str) -> (String, String) {
    let toolRaw = ExtractStr(line, "tool_name").unwrap_or_else(|| "Bash".to_string());
    let base = toolRaw.rsplit(':').next().unwrap_or(&toolRaw);
    if base == "call_mcp_tool" {
        return MapMcpPayload(line);
    }

    let params = ExtractObj(line, "parameters").unwrap_or_else(|| line.to_string());
    let rawPath = ExtractStr(&params, "Path")
        .or_else(|| ExtractStr(&params, "TargetFile"))
        .or_else(|| ExtractStr(&params, "AbsolutePath"))
        .or_else(|| ExtractStr(&params, "file_path"))
        .unwrap_or_default();
    let cleanPath = CleanDisplayPath(&rawPath);

    match base {
        "read_file" | "view_file" | "Read" => ("Read".to_string(), format!("{{\"file_path\":\"{}\"}}", EscapeJson(&cleanPath))),
        "write_file" | "write_to_file" | "Write" => ("Write".to_string(), format!("{{\"file_path\":\"{}\"}}", EscapeJson(&cleanPath))),
        "edit_file" | "replace_file_content" | "multi_replace_file_content" | "Edit" => ("Edit".to_string(), format!("{{\"file_path\":\"{}\"}}", EscapeJson(&cleanPath))),
        "grep_search" | "Grep" => {
            let q = ExtractStr(&params, "Query").or_else(|| ExtractStr(&params, "pattern")).unwrap_or_default();
            ("Grep".to_string(), format!("{{\"pattern\":\"{}\"}}", EscapeJson(&q)))
        }
        "find_by_name" | "list_dir" | "Glob" => {
            let pat = ExtractStr(&params, "Pattern").unwrap_or(cleanPath);
            ("Glob".to_string(), format!("{{\"pattern\":\"{}\"}}", EscapeJson(&pat)))
        }
        "invoke_subagent" | "Task" => {
            let role = ExtractStr(&params, "Role").unwrap_or_else(|| "Subagent".to_string());
            ("Task".to_string(), format!("{{\"subagent_type\":\"Explore\",\"description\":\"{}\"}}", EscapeJson(&role)))
        }
        "search_web" | "WebSearch" => {
            let q = ExtractStr(&params, "query").unwrap_or_default();
            ("WebSearch".to_string(), format!("{{\"query\":\"{}\"}}", EscapeJson(&q)))
        }
        "read_url_content" | "WebFetch" => {
            let u = ExtractStr(&params, "Url").unwrap_or_default();
            ("WebFetch".to_string(), format!("{{\"url\":\"{}\"}}", EscapeJson(&u)))
        }
        _ => {
            let cmd = ExtractStr(&params, "CommandLine")
                .or_else(|| ExtractStr(&params, "command"))
                .unwrap_or_else(|| base.to_string());
            ("Bash".to_string(), format!("{{\"command\":\"{}\"}}", EscapeJson(&cmd)))
        }
    }
}

fn NativeMcpAdapterNote() -> &'static str {
    concat!(
        "[Ropilot Native MCP Adapter — Authoritative]\n",
        "- All `ropilot_*`, `search_asset`, and `insert_asset` tools referenced below belong to MCP server `ropilot` (`call_mcp_tool` with `ServerName: \"ropilot\"`, `ToolName: \"<tool>\"`). Use `ropilot` MCP exclusively for all Roblox Studio, Source Control, Exploration, Screenshots, Playtest, Blender, and Asset operations.\n",
        "- Never substitute `Roblox_Studio` MCP tools or read MCP schema `.json` files from disk.\n",
        "- For `roblox-tester` subagent tasks, invoke `invoke_subagent` with `EnableMCPTools: true` or run `ropilot_test_*` tools on `ServerName: \"ropilot\"` directly."
    )
}

fn FlushText(state: &mut TurnState) {
    if state.textBuf.is_empty() { return; }
    let text = std::mem::take(&mut state.textBuf);
    EmitAssistantText(&text, state);
    state.emittedText = true;
}

fn HandleTool(line: &str, state: &mut TurnState) {
    FlushText(state);
    EnsureInit(state);
    let stepNum = ExtractNum(line, "step_index");
    let stepState = ExtractStr(line, "state").unwrap_or_default();
    let stepId = format!("toolu_{}", stepNum);

    if stepState == "ACTIVE" {
        let toolRaw = ExtractStr(line, "tool_name").unwrap_or_default();
        let base = toolRaw.rsplit(':').next().unwrap_or(&toolRaw);
        if matches!(base, "read_file" | "view_file" | "Read") {
            let params = ExtractObj(line, "parameters").unwrap_or_else(|| line.to_string());
            let rawPath = ExtractStr(&params, "Path")
                .or_else(|| ExtractStr(&params, "AbsolutePath"))
                .or_else(|| ExtractStr(&params, "file_path"))
                .unwrap_or_default();
            if IsImagePath(&rawPath) {
                state.suppressedSteps.insert(stepNum);
                EmitChatImage(&rawPath, stepNum, state);
                return;
            }
        }
        let (name, inputJson) = MapToolPayload(line);
        let evt = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"{}\",\"name\":\"{}\",\"input\":{}}}]}}}}\n",
            EscapeJson(&stepId), EscapeJson(&name), inputJson
        );
        EmitStdout(&evt, &state.stdoutLock);
    } else if stepState == "DONE" {
        let info = ExtractObj(line, "tool_info").unwrap_or_else(|| line.to_string());
        let out = ExtractStr(&info, "output").unwrap_or_default();
        if let Some(img) = ExtractOffloadedImage(&out) {
            EmitChatImage(&img, stepNum, state);
        }
        if state.suppressedSteps.remove(&stepNum) { return; }
        let evt = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"{}\",\"content\":\"{}\",\"is_error\":false}}]}}}}\n",
            EscapeJson(&stepId), EscapeJson(&out)
        );
        EmitStdout(&evt, &state.stdoutLock);
    }
}

fn EmitResult(isErr: bool, resultText: &str, inTok: u64, cacheTok: u64, outTok: u64, state: &mut TurnState) {
    EnsureInit(state);
    let subtype = if isErr { "error" } else { "success" };
    let evt = format!(
        "{{\"type\":\"result\",\"subtype\":\"{}\",\"is_error\":{},\"result\":\"{}\",\"session_id\":\"{}\",\"usage\":{{\"input_tokens\":{},\"cache_read_input_tokens\":{},\"output_tokens\":{}}},\"modelUsage\":{{\"{}\":{{\"inputTokens\":{},\"outputTokens\":{},\"cacheReadInputTokens\":{},\"contextWindow\":1000000,\"maxOutputTokens\":65536}}}}}}\n",
        subtype,
        if isErr { "true" } else { "false" },
        EscapeJson(resultText),
        EscapeJson(&state.sessionId),
        inTok, cacheTok, outTok,
        EscapeJson(&state.modelName), inTok, outTok, cacheTok
    );
    EmitStdout(&evt, &state.stdoutLock);
    state.inFlight = false;
    state.emittedText = false;
}

/// Translates one `agy` NDJSON stdout event into Claude `stream-json` events.
/// @param line Single stdout line from `agy`.
/// @param state Shared turn state.
fn HandleAgentLine(line: &str, state: &mut TurnState) {
    let trimmed = line.trim();
    if trimmed.is_empty() { return; }
    if state.convId.is_empty() {
        if let Some(cid) = ExtractStr(trimmed, "conversation_id") {
            state.convId = cid;
        }
    }
    if !state.convId.is_empty() && state.savedConvId != state.convId {
        SaveConvId(&state.sessionId, &state.convId);
        state.savedConvId = state.convId.clone();
    }

    if trimmed.contains("\"event\":\"init\"") {
        EnsureInit(state);
        return;
    }

    if trimmed.contains("\"step_type\":\"agent_response\"") {
        if let Some(delta) = ExtractStr(trimmed, "text_delta") {
            state.textBuf.push_str(&delta);
        }
        if trimmed.contains("\"state\":\"DONE\"") {
            FlushText(state);
        }
        return;
    }

    if trimmed.contains("\"step_type\":\"tool\"") {
        HandleTool(trimmed, state);
        return;
    }

    if trimmed.contains("\"event\":\"result\"") {
        let res = ExtractObj(trimmed, "result").unwrap_or_else(|| trimmed.to_string());
        let status = ExtractStr(&res, "status").unwrap_or_default();
        let errMsg = ExtractStr(&res, "error").unwrap_or_default();
        let resp = ExtractStr(&res, "response").unwrap_or_default();
        let isErr = status.eq_ignore_ascii_case("ERROR") || !errMsg.is_empty();

        FlushText(state);
        if !isErr && !state.emittedText && !resp.is_empty() {
            EmitAssistantText(&resp, state);
        }

        let inTok = ExtractNum(&res, "input_tokens");
        let outTok = ExtractNum(&res, "output_tokens") + ExtractNum(&res, "thinking_tokens");
        let cacheTok = ExtractNum(&res, "cache_read_tokens");
        let finalMsg = if isErr {
            if !errMsg.is_empty() { errMsg } else if !resp.is_empty() { resp } else { "Antigravity CLI returned an error.".to_string() }
        } else {
            resp
        };
        EmitResult(isErr, &finalMsg, inTok, cacheTok, outTok, state);
    }
}

fn SaveImage(mediaType: &str, b64: &str, idx: usize) -> Option<PathBuf> {
    let bytes = DecodeBase64(b64)?;
    if bytes.is_empty() { return None; }
    let ext = if mediaType.contains("jpeg") || mediaType.contains("jpg") { "jpg" } else if mediaType.contains("webp") { "webp" } else { "png" };
    let dir = env::temp_dir().join("ropilot-agy-inputs");
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("img_{}_{}.{}", NowMs(), idx, ext));
    fs::write(&path, bytes).ok()?;
    Some(path)
}

fn ExtractArrayTurn(arr: &str) -> Option<String> {
    let (s, e) = ExtractSpan(arr, '[', ']')?;
    let inner = &arr[s + 1..e - 1];
    let mut parts = Vec::new();
    let mut imgIdx = 0usize;
    let mut pos = 0usize;
    while pos < inner.len() {
        let Some((o0, o1)) = ExtractSpan(&inner[pos..], '{', '}') else { break; };
        let obj = &inner[pos + o0..pos + o1];
        let kind = ExtractStr(obj, "type").unwrap_or_default();
        if kind == "text" {
            if let Some(t) = ExtractStr(obj, "text") {
                if !t.is_empty() { parts.push(t); }
            }
        } else if kind == "image" {
            let src = ExtractObj(obj, "source").unwrap_or_else(|| obj.to_string());
            let mime = ExtractStr(&src, "media_type").unwrap_or_else(|| "image/png".to_string());
            if let Some(b64) = ExtractStr(&src, "data") {
                if let Some(saved) = SaveImage(&mime, &b64, imgIdx) {
                    imgIdx += 1;
                    parts.push(format!("[User attached image saved at: {} — inspect it with read_file]", saved.display()));
                }
            }
        }
        pos += o1;
    }
    if parts.is_empty() { None } else { Some(parts.join("\n")) }
}

fn ExtractUserBody(trimmed: &str) -> String {
    if let Some(val) = FindFieldValue(trimmed, "content") {
        if val.starts_with('[') {
            if let Some(parsed) = ExtractArrayTurn(val) {
                return parsed;
            }
        }
    }
    ExtractStr(trimmed, "prompt")
        .or_else(|| ExtractStr(trimmed, "text"))
        .or_else(|| ExtractStr(trimmed, "content"))
        .unwrap_or_else(|| trimmed.to_string())
}

struct AgyChild {
    child: Child,
    stdin: ChildStdin,
    _guard: Option<proc_guard::GuardHandle>,
    outThread: JoinHandle<()>,
    errThread: JoinHandle<()>,
}

impl AgyChild {
    fn Shutdown(mut self) -> i32 {
        drop(self.stdin);
        let _ = self.outThread.join();
        let _ = self.errThread.join();
        let _ = self.child.kill();
        self.child.wait().ok().and_then(|s| s.code()).unwrap_or(0)
    }
}

fn SpawnAgy(
    model: &str,
    convId: &str,
    mcpConfig: &str,
    genId: u64,
    state: Arc<Mutex<TurnState>>,
    tx: mpsc::Sender<BridgeEvent>,
) -> Option<AgyChild> {
    EnsureMcpRouter();
    let mut args = vec![
        "--input-format".to_string(), "stream-json".to_string(),
        "--output-format".to_string(), "stream-json".to_string(),
        "--model".to_string(), model.to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--disable-slash-commands".to_string(),
    ];
    if !convId.trim().is_empty() {
        args.push("--conversation".to_string());
        args.push(convId.trim().to_string());
    }

    let mut cmd = Command::new(ResolveAgyBin());
    cmd.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if !mcpConfig.trim().is_empty() {
        cmd.env("ROPILOT_SESSION_MCP_CONFIG", mcpConfig.trim());
    }
    let mut child = cmd.spawn().ok()?;
    let guard = proc_guard::AttachChild(&child);
    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;

    let errState = Arc::clone(&state);
    let errThread = thread::spawn(move || {
        for lineRes in BufReader::new(stderr).lines() {
            let Ok(line) = lineRes else { break; };
            let _ = writeln!(io::stderr(), "{}", line);
            if !line.trim().is_empty() {
                if let Ok(mut guard) = errState.lock() {
                    if guard.stderrTail.len() >= 12 { guard.stderrTail.remove(0); }
                    guard.stderrTail.push(line);
                }
            }
        }
    });

    let outThread = thread::spawn(move || {
        for lineRes in BufReader::new(stdout).lines() {
            let Ok(line) = lineRes else { break; };
            if let Ok(mut guard) = state.lock() {
                HandleAgentLine(&line, &mut guard);
            }
        }
        let _ = tx.send(BridgeEvent::ChildExited(genId, 1));
    });

    Some(AgyChild { child, stdin, _guard: guard, outThread, errThread })
}

fn EmitUnexpectedExit(stateArc: &Arc<Mutex<TurnState>>) {
    let Ok(mut guard) = stateArc.lock() else { return; };
    if !guard.inFlight && guard.stderrTail.is_empty() { return; }
    FlushText(&mut guard);
    let detail = if guard.stderrTail.is_empty() {
        "Antigravity CLI exited unexpectedly before returning a result.".to_string()
    } else {
        guard.stderrTail.join("\n")
    };
    EmitResult(true, &detail, 0, 0, 0, &mut guard);
}

/// Runs the bidirectional `stream-json` bridge loop between `ropilot-infra` and `agy`.
/// @param cfg Parsed CLI configuration.
/// @param initModel Initial resolved `agy` model ID.
/// @return Process exit code.
fn RunSession(cfg: BridgeCfg, initModel: String) -> i32 {
    WriteSessionMarker(&cfg.mcpConfig);
    let transcriptLock = Arc::new(Mutex::new(ResolveTranscript(&cfg.sessionId)));
    let sysPrompt = if !cfg.sysPromptFile.is_empty() {
        fs::read_to_string(&cfg.sysPromptFile).unwrap_or_default()
    } else {
        String::new()
    };

    let existingConv = LookupConvId(&cfg.sessionId).unwrap_or_default();
    let state = Arc::new(Mutex::new(TurnState {
        sessionId: cfg.sessionId.clone(),
        modelName: initModel.clone(),
        convId: existingConv.clone(),
        savedConvId: existingConv.clone(),
        initEmitted: false,
        inFlight: false,
        textBuf: String::new(),
        emittedText: false,
        suppressedSteps: HashSet::new(),
        emittedImages: HashSet::new(),
        stderrTail: Vec::new(),
        transcriptLock: Arc::clone(&transcriptLock),
        stdoutLock: Arc::new(Mutex::new(())),
    }));

    let (tx, rx) = mpsc::channel::<BridgeEvent>();
    let mut genId = 1u64;
    let Some(first) = SpawnAgy(&initModel, &existingConv, &cfg.mcpConfig, genId, Arc::clone(&state), tx.clone()) else {
        EmitUnexpectedExit(&state);
        ClearSessionMarker(&cfg.mcpConfig);
        return 1;
    };
    let mut active = Some(first);

    let stdinTx = tx.clone();
    thread::spawn(move || {
        for lineRes in io::stdin().lock().lines() {
            let Ok(line) = lineRes else { break; };
            if stdinTx.send(BridgeEvent::StdinLine(line)).is_err() { break; }
        }
        let _ = stdinTx.send(BridgeEvent::StdinClosed);
    });

    let mut curModel = initModel;
    let mut firstTurn = existingConv.is_empty();
    let mut exitCode = 0;

    while let Ok(evt) = rx.recv() {
        match evt {
            BridgeEvent::StdinClosed => break,
            BridgeEvent::ChildExited(id, code) => {
                if id != genId { continue; }
                EmitUnexpectedExit(&state);
                exitCode = code;
                break;
            }
            BridgeEvent::StdinLine(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() { continue; }

                if let Some(route) = ResolveRoute(&cfg) {
                    let nextModel = ResolveAgyModel(&route.model, &route.effort);
                    if nextModel != curModel {
                        genId += 1;
                        if let Some(prev) = active.take() {
                            let _ = prev.Shutdown();
                        }
                        let conv = state.lock().map(|g| g.convId.clone()).unwrap_or_default();
                        if let Ok(mut g) = state.lock() {
                            g.modelName = nextModel.clone();
                            g.stderrTail.clear();
                            g.textBuf.clear();
                        }
                        let Some(respawned) = SpawnAgy(&nextModel, &conv, &cfg.mcpConfig, genId, Arc::clone(&state), tx.clone()) else {
                            EmitUnexpectedExit(&state);
                            exitCode = 1;
                            break;
                        };
                        active = Some(respawned);
                        curModel = nextModel;
                        firstTurn = false;
                    }
                }

                let mut body = ExtractUserBody(trimmed);
                if let Ok(guard) = transcriptLock.lock() {
                    let rec = format!(
                        "{{\"type\":\"user\",\"sessionId\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}",
                        EscapeJson(&cfg.sessionId), EscapeJson(&body)
                    );
                    AppendTranscript(&*guard, &rec);
                }

                if firstTurn {
                    let header = if sysPrompt.trim().is_empty() {
                        NativeMcpAdapterNote().to_string()
                    } else {
                        format!("{}\n\n{}", NativeMcpAdapterNote(), sysPrompt.trim())
                    };
                    body = format!("{}\n\n[User Request]\n{}", header, body);
                    firstTurn = false;
                }

                if let Ok(mut g) = state.lock() {
                    g.inFlight = true;
                    g.emittedText = false;
                }
                let payload = format!("{{\"event\":\"user\",\"message\":{{\"content\":\"{}\"}}}}\n", EscapeJson(&body));
                let writeErr = match active.as_mut() {
                    Some(ch) => ch.stdin.write_all(payload.as_bytes()).is_err() || ch.stdin.flush().is_err(),
                    None => true,
                };
                if writeErr {
                    EmitUnexpectedExit(&state);
                    exitCode = 1;
                    break;
                }
            }
        }
    }

    let childCode = active.map(|a| a.Shutdown()).unwrap_or(0);
    ClearSessionMarker(&cfg.mcpConfig);
    if exitCode != 0 { exitCode } else { childCode }
}

fn ShouldRouteAgy(cfg: &BridgeCfg, route: &Option<RouteCfg>) -> bool {
    if cfg.model.trim().starts_with("agy:") { return true; }
    if let Some(r) = route {
        let p = r.provider.trim().to_ascii_lowercase();
        if matches!(p.as_str(), "gemini" | "agy" | "antigravity") { return true; }
        if p == "claude" { return false; }
    }
    !IsClaudeName(&cfg.model) && !cfg.model.trim().is_empty()
}

fn EmitMissingClaude(sessionId: &str, model: &str) {
    let lock = Arc::new(Mutex::new(()));
    EmitStdout(&format!("{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{}\"}}\n", EscapeJson(sessionId)), &lock);
    let msg = "Real Claude Code CLI is not installed on this machine. Switch to the Gemini (AGY) tab or install Claude Code.";
    EmitStdout(&format!("{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n", EscapeJson(msg)), &lock);
    EmitStdout(&format!(
        "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"{}\",\"session_id\":\"{}\",\"usage\":{{\"input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":0}},\"modelUsage\":{{\"{}\":{{\"inputTokens\":0,\"outputTokens\":0,\"cacheReadInputTokens\":0,\"contextWindow\":200000,\"maxOutputTokens\":16384}}}}}}\n",
        EscapeJson(msg), EscapeJson(sessionId), EscapeJson(model)
    ), &lock);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let cfg = ParseCfg(&args);

    if cfg.isVersionCheck {
        if let Some(code) = ForwardRealClaude(&args) { std::process::exit(code); }
        println!("2.0.0");
        return;
    }

    let route = ResolveRoute(&cfg);

    if let Some(ref prompt) = cfg.printPrompt {
        if IsAuthProbe(prompt) {
            println!("OK");
            return;
        }
        if !ShouldRouteAgy(&cfg, &route) {
            if let Some(code) = ForwardRealClaude(&args) { std::process::exit(code); }
            std::process::exit(1);
        }
        EnsureMcpRouter();
        let m = route.as_ref().map(|r| ResolveAgyModel(&r.model, &r.effort)).unwrap_or_else(|| ResolveAgyModel(&cfg.model, &cfg.effort));
        match Command::new(ResolveAgyBin()).args(["-p", prompt, "--model", &m, "--dangerously-skip-permissions", "--disable-slash-commands"]).output() {
            Ok(o) => {
                let _ = io::stdout().write_all(&o.stdout);
                let _ = io::stderr().write_all(&o.stderr);
                std::process::exit(o.status.code().unwrap_or(0));
            }
            Err(err) => {
                eprintln!("Failed to run agy: {}", err);
                std::process::exit(1);
            }
        }
    }

    if !cfg.isStreamJson {
        println!("OK");
        return;
    }

    if !ShouldRouteAgy(&cfg, &route) {
        if let Some(code) = ForwardRealClaude(&args) { std::process::exit(code); }
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        EmitMissingClaude(&cfg.sessionId, &cfg.model);
        return;
    }

    let target = match route {
        Some(r) => ResolveAgyModel(&r.model, &r.effort),
        None => ResolveAgyModel(&cfg.model, &cfg.effort),
    };
    std::process::exit(RunSession(cfg, target));
}