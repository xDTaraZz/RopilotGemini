#![allow(non_snake_case)]
use std::collections::{HashSet, VecDeque};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static IMAGE_SEQ: AtomicU64 = AtomicU64::new(1);
static MODELS_CACHE: Mutex<Option<Vec<String>>> = Mutex::new(None);

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

    /// Binds a spawned child process to a Windows Job Object so it exits with the bridge.
    /// @param child Spawned `agy` child process.
    /// @return RAII Job Object handle.
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

#[cfg(not(windows))]
mod proc_guard {
    use std::process::Child;
    pub struct GuardHandle;
    pub fn AttachChild(_child: &Child) -> Option<GuardHandle> { None }
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
    skipPermissions: bool,
}

struct RouteCfg {
    provider: String,
    model: String,
    effort: String,
}

struct TurnState {
    sessionId: String,
    wireModel: String,
    modelName: String,
    convId: String,
    savedConvId: String,
    initEmitted: bool,
    inFlight: bool,
    textBuf: String,
    emittedText: bool,
    activeSteps: HashSet<u64>,
    stderrTail: Vec<String>,
    transcriptLock: Arc<Mutex<Option<PathBuf>>>,
    stdoutLock: Arc<Mutex<()>>,
}

enum BridgeEvent {
    StdinLine(String),
    StdinClosed,
    TurnDone(u64),
    StdoutClosed(u64),
}

fn NonEmptyEnv(key: &str) -> Option<String> {
    env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Resolves Ropilot Studio's per-user data directory across Windows, macOS, and Linux.
/// @return Path to the `ropilot` userData directory.
fn DataDir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = NonEmptyEnv("APPDATA")
            .or_else(|| NonEmptyEnv("USERPROFILE").map(|h| PathBuf::from(h).join("AppData").join("Roaming").to_string_lossy().to_string()))
            .unwrap_or_default();
        return PathBuf::from(base).join("ropilot");
    }
    #[cfg(target_os = "macos")]
    {
        return HomeDir().join("Library").join("Application Support").join("ropilot");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let cfg = NonEmptyEnv("XDG_CONFIG_HOME")
            .unwrap_or_else(|| HomeDir().join(".config").to_string_lossy().to_string());
        return PathBuf::from(cfg).join("ropilot");
    }
}

/// Resolves the current user's home directory across Windows and POSIX platforms.
/// @return Path to the user's home directory.
fn HomeDir() -> PathBuf {
    #[cfg(windows)]
    {
        let raw = NonEmptyEnv("USERPROFILE").or_else(|| NonEmptyEnv("HOME")).unwrap_or_default();
        return PathBuf::from(raw);
    }
    #[cfg(not(windows))]
    {
        return PathBuf::from(NonEmptyEnv("HOME").unwrap_or_default());
    }
}

fn NowMs() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
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
fn ParseJsonString(slice: &str) -> Option<(String, usize)> {
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
                    if let Some(c) = char::from_u32(hi as u32) { out.push(c); }
                } else {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    None
}

/// Finds a top-level field value (`depth == 1`) inside a JSON object.
/// @param json Enclosing JSON object text.
/// @param key Exact field name to locate.
/// @return Value slice immediately after `:`.
fn FindFieldValue<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let bytes = json.as_bytes();
    let mut pos = 0usize;
    let mut depth = 0usize;
    while pos < bytes.len() {
        match bytes[pos] {
            b'{' | b'[' => { depth += 1; pos += 1; }
            b'}' | b']' => { depth = depth.saturating_sub(1); pos += 1; }
            b'"' => {
                let (name, len) = ParseJsonString(&json[pos..])?;
                if depth == 1 && name == key {
                    if let Some(val) = json[pos + len..].trim_start().strip_prefix(':') {
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

fn ExtractStr(json: &str, key: &str) -> Option<String> {
    let val = FindFieldValue(json, key)?;
    if !val.starts_with('"') { return None; }
    ParseJsonString(val).map(|(s, _)| s)
}

fn ExtractSpan(text: &str, open: char, close: char) -> Option<(usize, usize)> {
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
                return Some((0, idx + ch.len_utf8()));
            }
        }
    }
    None
}

fn ExtractObj(json: &str, key: &str) -> Option<String> {
    let val = FindFieldValue(json, key)?;
    let (s, e) = ExtractSpan(val, '{', '}')?;
    Some(val[s..e].to_string())
}

fn ExtractStrOrObj(json: &str, key: &str) -> Option<String> {
    let val = FindFieldValue(json, key)?;
    if let Some((s, e)) = ExtractSpan(val, '{', '}') {
        return Some(val[s..e].to_string());
    }
    if val.starts_with('"') {
        let (decoded, _) = ParseJsonString(val)?;
        let trimmed = decoded.trim();
        if let Some((s, e)) = ExtractSpan(trimmed, '{', '}') {
            if e == trimmed.len() {
                return Some(trimmed[s..e].to_string());
            }
        }
    }
    None
}

fn ExtractNum(json: &str, key: &str) -> u64 {
    let Some(val) = FindFieldValue(json, key) else { return 0; };
    let digits: String = val.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().unwrap_or(0)
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

/// Parses Claude CLI flags passed by `ropilot-infra` without misinterpreting option values.
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
        skipPermissions: NonEmptyEnv("ROPILOT_GEMINI_SKIP_PERMISSIONS").as_deref() == Some("1"),
    };

    let mut printMode = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1usize;

    while i < args.len() {
        let tok = args[i].as_str();
        if tok == "--version" || tok == "-v" {
            cfg.isVersionCheck = true;
            i += 1;
            continue;
        }
        if tok == "--dangerously-skip-permissions" {
            cfg.skipPermissions = true;
            i += 1;
            continue;
        }
        if tok == "-p" || tok == "--print" {
            printMode = true;
            i += 1;
            continue;
        }

        let (flag, inlineVal) = match tok.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (tok, None),
        };

        let takesVal = matches!(
            flag,
            "--session-id" | "--resume" | "--system-prompt-file" | "--model"
                | "--effort" | "--mcp-config" | "--output-format" | "--input-format"
                | "--permission-mode" | "--disallowedTools" | "--allowedTools" | "--max-turns"
        );
        if takesVal {
            let valOpt = inlineVal.or_else(|| {
                let next = args.get(i + 1).map(String::as_str);
                if next.is_some() { i += 1; }
                next
            });
            if let Some(val) = valOpt {
                match flag {
                    "--session-id" | "--resume" => cfg.sessionId = val.to_string(),
                    "--system-prompt-file" => cfg.sysPromptFile = val.to_string(),
                    "--model" => cfg.model = val.to_string(),
                    "--effort" => cfg.effort = val.to_string(),
                    "--mcp-config" => cfg.mcpConfig = val.to_string(),
                    "--output-format" | "--input-format" if val == "stream-json" => cfg.isStreamJson = true,
                    _ => {}
                }
            }
            i += 1;
            continue;
        }

        if let Some(val) = inlineVal {
            if flag == "-p" || flag == "--print" {
                printMode = true;
                cfg.printPrompt = Some(val.to_string());
            }
        } else if !tok.starts_with('-') {
            positional.push(tok.to_string());
        }
        i += 1;
    }

    if printMode && cfg.printPrompt.is_none() && !cfg.isStreamJson {
        cfg.printPrompt = positional.into_iter().next();
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
    if provider.is_empty() && model.is_empty() { return None; }
    Some(RouteCfg { provider, model, effort })
}

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
/// @return Path to `agy` executable or command name on `PATH`.
fn ResolveAgyBin() -> PathBuf {
    let binName = if cfg!(windows) { "agy.exe" } else { "agy" };
    let candidates = [
        HomeDir().join(".gemini").join("bin").join(binName),
        PathBuf::from("/opt/homebrew/bin").join(binName),
        PathBuf::from("/usr/local/bin").join(binName),
    ];
    for cand in candidates {
        if cand.is_file() { return cand; }
    }
    PathBuf::from(binName)
}

fn LiveAgyModels() -> Vec<String> {
    if let Ok(guard) = MODELS_CACHE.lock() {
        if let Some(ref cached) = *guard {
            return cached.clone();
        }
    }
    let Ok(out) = Command::new(ResolveAgyBin()).arg("models").output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let list: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split('\t').next())
        .map(str::trim)
        .filter(|id| !id.is_empty() && !id.starts_with("Fetching"))
        .map(str::to_string)
        .collect();
    if !list.is_empty() {
        if let Ok(mut guard) = MODELS_CACHE.lock() {
            *guard = Some(list.clone());
        }
    }
    list
}

fn StripEffortSuffix(model: &str) -> &str {
    for sfx in ["-low", "-medium", "-high", "-xhigh"] {
        if let Some(base) = model.strip_suffix(sfx) {
            return base;
        }
    }
    model
}

/// Resolves a UI model ID and reasoning effort against live `agy models` without silent cross-model substitution.
/// @param rawModel Base or suffixed model ID.
/// @param rawEffort Effort tier (`low`, `medium`, `high`, `xhigh`, `max`).
/// @return Resolved `agy` model ID or descriptive error when unavailable.
fn ResolveAgyModel(rawModel: &str, rawEffort: &str) -> Result<String, String> {
    let stripped = rawModel.trim().strip_prefix("agy:").unwrap_or(rawModel.trim());
    if stripped.is_empty() {
        return Err("No Gemini model was selected.".to_string());
    }
    let models = LiveAgyModels();
    let effort = rawEffort.trim().to_ascii_lowercase();
    let mappedEffort = if effort == "max" { "xhigh" } else { effort.as_str() };
    let base = if effort.is_empty() { stripped } else { StripEffortSuffix(stripped) };

    if models.is_empty() {
        return Ok(if mappedEffort.is_empty() || base.contains("orchestrator") {
            base.to_string()
        } else {
            format!("{}-{}", base, mappedEffort)
        });
    }

    if !mappedEffort.is_empty() {
        let exact = format!("{}-{}", base, mappedEffort);
        if models.iter().any(|m| m == &exact) {
            return Ok(exact);
        }
        if mappedEffort == "xhigh" {
            let highFallback = format!("{}-high", base);
            if models.iter().any(|m| m == &highFallback) {
                return Ok(highFallback);
            }
        }
    }

    if models.iter().any(|m| m == base) {
        return Ok(base.to_string());
    }
    if let Some(sameBase) = models.iter().find(|m| StripEffortSuffix(m) == base) {
        return Ok(sameBase.clone());
    }

    Err(format!(
        "Model '{}' is not available in your installed Antigravity CLI (`agy models`). Available models: {}",
        base,
        models.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
    ))
}

fn FindRealClaude() -> Option<PathBuf> {
    let realName = if cfg!(windows) { "claude-real.exe" } else { "claude-real" };
    let managed = DataDir().join("claude").join(realName);
    if managed.is_file() { return Some(managed); }
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
    let tail = if args.len() > 1 { &args[1..] } else { &[] };
    let status = Command::new(real)
        .args(tail)
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

fn WriteFileAtomic(path: &Path, content: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let seq = IMAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), seq));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

fn BackupOriginalMcpEntry(cfgPath: &Path, entrySlice: &str) {
    let backupPath = DataDir().join("mcp-backup.json");
    let existing = fs::read_to_string(&backupPath).unwrap_or_else(|_| "{}".to_string());
    let key = cfgPath.to_string_lossy().to_string();
    if existing.contains(&EscapeJson(&key)) {
        return;
    }
    let entry = format!("\"{}\":{}", EscapeJson(&key), entrySlice);
    let trimmed = existing.trim();
    let updated = if trimmed == "{}" || trimmed.len() < 2 {
        format!("{{{}}}", entry)
    } else if let Some(body) = trimmed.strip_suffix('}') {
        format!("{},{}}}", body, entry)
    } else {
        format!("{{{}}}", entry)
    };
    let _ = WriteFileAtomic(&backupPath, updated.as_bytes());
}

fn PatchMcpConfig(cfgPath: &Path, routerPath: &Path, dataDir: &Path) {
    let Ok(raw) = fs::read_to_string(cfgPath) else { return; };
    if !raw.contains("ropilot-infra-helper") { return; }

    let Some(sVal) = FindFieldValue(&raw, "mcpServers") else { return; };
    let Some((s0, s1)) = ExtractSpan(sVal, '{', '}') else { return; };
    let sAbs = sVal.as_ptr() as usize - raw.as_ptr() as usize;
    let serversSlice = &raw[sAbs + s0..sAbs + s1];

    let Some(rVal) = FindFieldValue(serversSlice, "ropilot") else { return; };
    let Some((r0, r1)) = ExtractSpan(rVal, '{', '}') else { return; };
    let rAbs = rVal.as_ptr() as usize - raw.as_ptr() as usize;
    let origEntry = &raw[rAbs + r0..rAbs + r1];
    if !origEntry.contains("ropilot-infra-helper") { return; }

    BackupOriginalMcpEntry(cfgPath, origEntry);

    let repl = format!(
        "{{\"command\":\"{}\",\"args\":[],\"env\":{{\"ROPILOT_USERDATA\":\"{}\"}}}}",
        EscapeJson(&routerPath.to_string_lossy()),
        EscapeJson(&dataDir.to_string_lossy())
    );
    let updated = format!("{}{}{}", &raw[..rAbs + r0], repl, &raw[rAbs + r1..]);
    let _ = WriteFileAtomic(cfgPath, updated.as_bytes());
}

fn EnsureMcpRouter() {
    let dataDir = DataDir();
    let routerName = if cfg!(windows) { "ropilot-mcp-router.exe" } else { "ropilot-mcp-router" };
    let router = dataDir.join("bin").join(routerName);
    if !router.is_file() { return; }
    let gemDir = HomeDir().join(".gemini");
    PatchMcpConfig(&gemDir.join("antigravity").join("mcp_config.json"), &router, &dataDir);
    PatchMcpConfig(&gemDir.join("config").join("mcp_config.json"), &router, &dataDir);
}

fn WriteSessionMarker(mcpConfig: &str) {
    let s = mcpConfig.trim();
    if s.is_empty() { return; }
    let _ = WriteFileAtomic(&DataDir().join("agy-session-mcp.txt"), s.as_bytes());
}

fn ClearSessionMarker(mcpConfig: &str) {
    let marker = DataDir().join("agy-session-mcp.txt");
    if let Ok(cur) = fs::read_to_string(&marker) {
        if cur.trim() == mcpConfig.trim() {
            let _ = fs::remove_file(marker);
        }
    }
}

/// Encodes CWD matching `ropilot-infra` (`encodeClaudeProjectDir`: preserves `_` and single `.`).
/// @param cwd Working directory string.
/// @return Slug used under `~/.claude/projects/`.
fn EncodeClaudeSlug(cwd: &str) -> String {
    let norm = cwd.replace('\\', "/").replace("..", "--");
    norm.chars()
        .map(|c| match c {
            '/' | ':' | ' ' => '-',
            _ => c,
        })
        .collect()
}

fn SanitizeSessionId(sessionId: &str) -> Option<String> {
    let clean: String = sessionId
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if clean.is_empty() { None } else { Some(clean) }
}

fn ResolveTranscript(sessionId: &str) -> Option<PathBuf> {
    let safeId = SanitizeSessionId(sessionId)?;
    let cwd = env::current_dir().ok()?.to_string_lossy().to_string();
    let encoded = EncodeClaudeSlug(&cwd);
    let dir = HomeDir().join(".claude").join("projects").join(encoded);
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{}.jsonl", safeId)))
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
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}={}", sessionId.trim(), convId.trim());
    }
}

fn EvictConvId(sessionId: &str) {
    let target = sessionId.trim();
    if target.is_empty() { return; }
    let path = DataDir().join("session_map.txt");
    let Ok(raw) = fs::read_to_string(&path) else { return; };
    let kept: Vec<&str> = raw
        .lines()
        .filter(|l| l.split_once('=').map(|(k, _)| k.trim() != target).unwrap_or(true))
        .collect();
    let _ = WriteFileAtomic(&path, (kept.join("\n") + "\n").as_bytes());
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
    if let Ok(guard) = state.transcriptLock.lock() {
        if let Some(ref p) = *guard {
            if !p.exists() {
                let rec = format!("{{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"{}\"}}", EscapeJson(&state.sessionId));
                AppendTranscript(&*guard, &rec);
            }
        }
    }
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
    let server = ExtractStr(&params, "ServerName").unwrap_or_default();
    let inner = ExtractStr(&params, "ToolName").unwrap_or_else(|| "mcp_tool".to_string());
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

/// Translates `agy` tool step events (`ACTIVE`, `DONE`, `ERROR`, `FAILED`, `CANCELED`) into Claude `stream-json`.
/// @param line Single `step_type: "tool"` JSON line.
/// @param state Mutable turn state.
fn HandleTool(line: &str, state: &mut TurnState) {
    FlushText(state);
    EnsureInit(state);
    let stepNum = ExtractNum(line, "step_index");
    let stepState = ExtractStr(line, "state").unwrap_or_default();
    let stepId = format!("toolu_{}", stepNum);

    if stepState == "ACTIVE" {
        if !state.activeSteps.insert(stepNum) { return; }
        let (name, inputJson) = MapToolPayload(line);
        let safeInput = if inputJson.trim().starts_with('{') { inputJson } else { "{}".to_string() };
        let evt = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"{}\",\"name\":\"{}\",\"input\":{}}}]}}}}\n",
            EscapeJson(&stepId), EscapeJson(&name), safeInput
        );
        EmitStdout(&evt, &state.stdoutLock);
    } else if matches!(stepState.as_str(), "DONE" | "ERROR" | "FAILED" | "CANCELED" | "INVALID") {
        state.activeSteps.remove(&stepNum);
        let info = ExtractObj(line, "tool_info").unwrap_or_else(|| line.to_string());
        let failed = stepState != "DONE";
        let out = if failed {
            let errObj = ExtractObj(&info, "error").unwrap_or_else(|| info.clone());
            let msg = ExtractStr(&errObj, "message")
                .or_else(|| ExtractStr(&info, "output"))
                .unwrap_or_else(|| format!("Tool execution failed ({})", stepState));
            if msg.to_ascii_lowercase().contains("error") || msg.to_ascii_lowercase().contains("failed") {
                msg
            } else {
                format!("error: {}", msg)
            }
        } else {
            ExtractStr(&info, "output").unwrap_or_default()
        };
        let evt = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"{}\",\"content\":\"{}\",\"is_error\":{}}}]}}}}\n",
            EscapeJson(&stepId), EscapeJson(&out), if failed { "true" } else { "false" }
        );
        EmitStdout(&evt, &state.stdoutLock);
    }
}

fn EmitResult(isErr: bool, resultText: &str, inTok: u64, cacheTok: u64, outTok: u64, state: &mut TurnState) {
    EnsureInit(state);
    let subtype = if isErr { "error" } else { "success" };
    let wireKey = if state.wireModel.is_empty() { &state.modelName } else { &state.wireModel };
    let modelUsage = if wireKey == &state.modelName {
        format!(
            "\"{}\":{{\"inputTokens\":{},\"outputTokens\":{},\"cacheReadInputTokens\":{},\"contextWindow\":1000000,\"maxOutputTokens\":65536}}",
            EscapeJson(wireKey), inTok, outTok, cacheTok
        )
    } else {
        format!(
            "\"{}\":{{\"inputTokens\":{},\"outputTokens\":{},\"cacheReadInputTokens\":{},\"contextWindow\":1000000,\"maxOutputTokens\":65536}},\"{}\":{{\"inputTokens\":{},\"outputTokens\":{},\"cacheReadInputTokens\":{},\"contextWindow\":1000000,\"maxOutputTokens\":65536}}",
            EscapeJson(wireKey), inTok, outTok, cacheTok,
            EscapeJson(&state.modelName), inTok, outTok, cacheTok
        )
    };
    let evt = format!(
        "{{\"type\":\"result\",\"subtype\":\"{}\",\"is_error\":{},\"result\":\"{}\",\"session_id\":\"{}\",\"usage\":{{\"input_tokens\":{},\"cache_read_input_tokens\":{},\"output_tokens\":{}}},\"modelUsage\":{{{}}}}}\n",
        subtype,
        if isErr { "true" } else { "false" },
        EscapeJson(resultText),
        EscapeJson(&state.sessionId),
        inTok, cacheTok, outTok,
        modelUsage
    );
    EmitStdout(&evt, &state.stdoutLock);
    state.inFlight = false;
    state.emittedText = false;
    state.activeSteps.clear();
    state.stderrTail.clear();
}

/// Translates one `agy` NDJSON stdout event into Claude `stream-json` events.
/// @param line Single stdout line from `agy`.
/// @param state Shared turn state.
/// @return `true` when the active turn completed.
fn HandleAgentLine(line: &str, state: &mut TurnState) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() { return false; }

    if state.convId.is_empty() {
        if let Some(cid) = ExtractStr(trimmed, "conversation_id") {
            state.convId = cid;
        }
    }
    if !state.convId.is_empty() && state.savedConvId != state.convId {
        SaveConvId(&state.sessionId, &state.convId);
        state.savedConvId = state.convId.clone();
    }

    let eventType = ExtractStr(trimmed, "event").unwrap_or_default();
    if eventType == "init" {
        EnsureInit(state);
        return false;
    }

    let stepType = ExtractStr(trimmed, "step_type").unwrap_or_default();
    if stepType == "agent_response" {
        if let Some(delta) = ExtractStr(trimmed, "text_delta") {
            state.textBuf.push_str(&delta);
        }
        if ExtractStr(trimmed, "state").as_deref() == Some("DONE") {
            FlushText(state);
        }
        return false;
    }

    if stepType == "tool" {
        HandleTool(trimmed, state);
        return false;
    }

    if eventType == "result" {
        let res = ExtractObj(trimmed, "result").unwrap_or_else(|| trimmed.to_string());
        let status = ExtractStr(&res, "status").unwrap_or_default();
        let errMsg = ExtractStr(&res, "error").unwrap_or_default();
        let resp = ExtractStr(&res, "response").unwrap_or_default();
        let upper = status.to_ascii_uppercase();
        let isErr = !errMsg.is_empty() || matches!(upper.as_str(), "ERROR" | "INVALID" | "CANCELED" | "INTERRUPTED");

        FlushText(state);
        if isErr {
            let lowerErr = format!("{} {}", errMsg, resp).to_ascii_lowercase();
            if lowerErr.contains("run state not found") || lowerErr.contains("trajectory") {
                EvictConvId(&state.sessionId);
                state.convId.clear();
                state.savedConvId.clear();
            }
        } else if !state.emittedText && !resp.is_empty() {
            EmitAssistantText(&resp, state);
            state.emittedText = true;
        }

        let inTok = ExtractNum(&res, "input_tokens");
        let outTok = ExtractNum(&res, "output_tokens") + ExtractNum(&res, "thinking_tokens");
        let cacheTok = ExtractNum(&res, "cache_read_tokens");
        let finalMsg = if isErr {
            if !errMsg.is_empty() { errMsg } else if !resp.is_empty() { resp } else { format!("Antigravity CLI failed (status: {}).", status) }
        } else if !state.emittedText && resp.trim().is_empty() {
            "Done.".to_string()
        } else {
            resp
        };
        EmitResult(isErr, &finalMsg, inTok, cacheTok, outTok, state);
        return true;
    }
    false
}

/// Saves an attached user image into a private directory using exclusive file creation.
/// @param mediaType MIME type (`image/png`, `image/jpeg`, `image/webp`).
/// @param b64 Base64-encoded image bytes.
/// @param idx Attachment index within the turn.
/// @return Path to the saved file if valid.
fn SaveImage(mediaType: &str, b64: &str, idx: usize) -> Option<PathBuf> {
    let bytes = DecodeBase64(b64)?;
    if bytes.is_empty() { return None; }
    let ext = if mediaType.contains("jpeg") || mediaType.contains("jpg") { "jpg" } else if mediaType.contains("webp") { "webp" } else { "png" };
    let dir = DataDir().join("agy-inputs");
    fs::create_dir_all(&dir).ok()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }

    let seq = IMAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("img_{}_{}_{}_{}.{}", std::process::id(), NowMs(), seq, idx, ext));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path).ok()?;
    f.write_all(&bytes).ok()?;
    Some(path)
}

fn ExtractArrayTurn(arr: &str) -> Option<String> {
    let (s, e) = ExtractSpan(arr, '[', ']')?;
    let inner = &arr[s + 1..e - 1];
    let mut parts = Vec::new();
    let mut imgIdx = 0usize;
    let mut pos = 0usize;
    while pos < inner.len() {
        let Some(off) = inner[pos..].find('{') else { break; };
        let Some((o0, o1)) = ExtractSpan(&inner[pos + off..], '{', '}') else { break; };
        let obj = &inner[pos + off + o0..pos + off + o1];
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
        pos += off + o1;
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
    stdin: Option<ChildStdin>,
    _guard: Option<proc_guard::GuardHandle>,
    outThread: Option<JoinHandle<()>>,
    errThread: Option<JoinHandle<()>>,
}

impl AgyChild {
    /// Gracefully closes stdin, waits briefly, kills if still running, and then joins I/O threads.
    /// @return Child exit code.
    fn Shutdown(mut self) -> i32 {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_millis(250);
        let mut exitCode = None;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                exitCode = status.code();
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        if exitCode.is_none() {
            let _ = self.child.kill();
            exitCode = self.child.wait().ok().and_then(|s| s.code());
        }
        if let Some(t) = self.outThread.take() { let _ = t.join(); }
        if let Some(t) = self.errThread.take() { let _ = t.join(); }
        exitCode.unwrap_or(1)
    }
}

impl Drop for AgyChild {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn ApplyModeArgs(args: &mut Vec<String>, skipPermissions: bool) {
    if skipPermissions {
        args.push("--dangerously-skip-permissions".to_string());
    } else {
        args.push("--mode".to_string());
        args.push("accept-edits".to_string());
    }
}

fn SpawnAgy(
    model: &str,
    convId: &str,
    mcpConfig: &str,
    skipPermissions: bool,
    genId: u64,
    state: Arc<Mutex<TurnState>>,
    tx: mpsc::Sender<BridgeEvent>,
) -> Option<AgyChild> {
    EnsureMcpRouter();
    let mut args = vec![
        "--input-format".to_string(), "stream-json".to_string(),
        "--output-format".to_string(), "stream-json".to_string(),
        "--model".to_string(), model.to_string(),
        "--disable-slash-commands".to_string(),
    ];
    ApplyModeArgs(&mut args, skipPermissions);
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
    let Some(stdin) = child.stdin.take() else { let _ = child.kill(); let _ = child.wait(); return None; };
    let Some(stdout) = child.stdout.take() else { let _ = child.kill(); let _ = child.wait(); return None; };
    let Some(stderr) = child.stderr.take() else { let _ = child.kill(); let _ = child.wait(); return None; };

    let errState = Arc::clone(&state);
    let errThread = thread::spawn(move || {
        for lineRes in BufReader::new(stderr).lines() {
            let Ok(line) = lineRes else { break; };
            let _ = writeln!(io::stderr(), "{}", line);
            if !line.trim().is_empty() {
                if let Ok(mut g) = errState.lock() {
                    if g.stderrTail.len() >= 12 { g.stderrTail.remove(0); }
                    g.stderrTail.push(line);
                }
            }
        }
    });

    let outThread = thread::spawn(move || {
        for lineRes in BufReader::new(stdout).lines() {
            let Ok(line) = lineRes else { break; };
            let finished = state.lock().map(|mut g| HandleAgentLine(&line, &mut g)).unwrap_or(false);
            if finished {
                let _ = tx.send(BridgeEvent::TurnDone(genId));
            }
        }
        let _ = tx.send(BridgeEvent::StdoutClosed(genId));
    });

    Some(AgyChild { child, stdin: Some(stdin), _guard: guard, outThread: Some(outThread), errThread: Some(errThread) })
}

fn EmitFatalError(stateArc: &Arc<Mutex<TurnState>>, detail: &str) {
    let Ok(mut guard) = stateArc.lock() else { return; };
    FlushText(&mut guard);
    EmitAssistantText(detail, &mut guard);
    EmitResult(true, detail, 0, 0, 0, &mut guard);
}

fn EmitUnexpectedExit(stateArc: &Arc<Mutex<TurnState>>, code: i32) {
    let Ok(mut guard) = stateArc.lock() else { return; };
    if !guard.inFlight && code == 0 { return; }
    if !guard.inFlight && guard.stderrTail.is_empty() { return; }
    FlushText(&mut guard);
    let detail = if guard.stderrTail.is_empty() {
        format!("Antigravity CLI exited unexpectedly (code {}).", code)
    } else {
        guard.stderrTail.join("\n")
    };
    EmitResult(true, &detail, 0, 0, 0, &mut guard);
}

fn DispatchTurn(
    raw: &str,
    cfg: &BridgeCfg,
    active: &mut Option<AgyChild>,
    curModel: &mut String,
    firstTurn: &mut bool,
    genId: &mut u64,
    state: &Arc<Mutex<TurnState>>,
    tx: &mpsc::Sender<BridgeEvent>,
    sysPrompt: &str,
    transcriptLock: &Arc<Mutex<Option<PathBuf>>>,
) -> Result<(), String> {
    if let Some(route) = ResolveRoute(cfg) {
        let nextModel = ResolveAgyModel(&route.model, &route.effort)?;
        if nextModel != *curModel {
            *genId += 1;
            if let Some(prev) = active.take() {
                let _ = prev.Shutdown();
            }
            let conv = state.lock().map(|g| g.convId.clone()).unwrap_or_default();
            if let Ok(mut g) = state.lock() {
                g.modelName = nextModel.clone();
                g.stderrTail.clear();
                g.textBuf.clear();
            }
            let respawned = SpawnAgy(&nextModel, &conv, &cfg.mcpConfig, cfg.skipPermissions, *genId, Arc::clone(state), tx.clone())
                .ok_or_else(|| "Failed to respawn Antigravity CLI (`agy`).".to_string())?;
            *active = Some(respawned);
            *curModel = nextModel;
            if !conv.is_empty() { *firstTurn = false; }
        }
    }

    let mut body = ExtractUserBody(raw);
    if let Ok(guard) = transcriptLock.lock() {
        let rec = format!(
            "{{\"type\":\"user\",\"sessionId\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}",
            EscapeJson(&cfg.sessionId), EscapeJson(&body)
        );
        AppendTranscript(&*guard, &rec);
    }

    if *firstTurn {
        let header = if sysPrompt.trim().is_empty() {
            NativeMcpAdapterNote().to_string()
        } else {
            format!("{}\n\n{}", NativeMcpAdapterNote(), sysPrompt.trim())
        };
        body = format!("{}\n\n[User Request]\n{}", header, body);
        *firstTurn = false;
    }

    if let Ok(mut g) = state.lock() {
        g.inFlight = true;
        g.emittedText = false;
    }
    let payload = format!("{{\"event\":\"user\",\"message\":{{\"content\":\"{}\"}}}}\n", EscapeJson(&body));
    let writeErr = match active.as_mut().and_then(|c| c.stdin.as_mut()) {
        Some(stdin) => stdin.write_all(payload.as_bytes()).is_err() || stdin.flush().is_err(),
        None => true,
    };
    if writeErr {
        return Err("Failed to send prompt to Antigravity CLI stdin.".to_string());
    }
    Ok(())
}

/// Runs the bidirectional `stream-json` bridge loop with turn queuing and safe shutdown.
/// @param cfg Parsed CLI configuration.
/// @param initModel Resolved initial `agy` model ID.
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
        wireModel: cfg.model.clone(),
        modelName: initModel.clone(),
        convId: existingConv.clone(),
        savedConvId: existingConv.clone(),
        initEmitted: false,
        inFlight: false,
        textBuf: String::new(),
        emittedText: false,
        activeSteps: HashSet::new(),
        stderrTail: Vec::new(),
        transcriptLock: Arc::clone(&transcriptLock),
        stdoutLock: Arc::new(Mutex::new(())),
    }));

    let (tx, rx) = mpsc::channel::<BridgeEvent>();
    let mut genId = 1u64;
    let Some(first) = SpawnAgy(&initModel, &existingConv, &cfg.mcpConfig, cfg.skipPermissions, genId, Arc::clone(&state), tx.clone()) else {
        EmitFatalError(&state, "Failed to start Antigravity CLI (`agy`). Verify `agy` is installed and signed in.");
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
    let mut pendingLines: VecDeque<String> = VecDeque::new();
    let mut exitCode = 0;

    while let Ok(evt) = rx.recv() {
        match evt {
            BridgeEvent::StdinClosed => break,
            BridgeEvent::StdoutClosed(id) => {
                if id != genId { continue; }
                let code = active.take().map(|a| a.Shutdown()).unwrap_or(1);
                EmitUnexpectedExit(&state, code);
                exitCode = code;
                break;
            }
            BridgeEvent::TurnDone(id) => {
                if id != genId { continue; }
                if let Some(nextLine) = pendingLines.pop_front() {
                    if let Err(err) = DispatchTurn(&nextLine, &cfg, &mut active, &mut curModel, &mut firstTurn, &mut genId, &state, &tx, &sysPrompt, &transcriptLock) {
                        EmitFatalError(&state, &err);
                        exitCode = 1;
                        break;
                    }
                }
            }
            BridgeEvent::StdinLine(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() { continue; }
                let busy = state.lock().map(|g| g.inFlight).unwrap_or(false);
                if busy {
                    pendingLines.push_back(trimmed.to_string());
                    continue;
                }
                if let Err(err) = DispatchTurn(trimmed, &cfg, &mut active, &mut curModel, &mut firstTurn, &mut genId, &state, &tx, &sysPrompt, &transcriptLock) {
                    EmitFatalError(&state, &err);
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

fn EmitStaticError(sessionId: &str, model: &str, msg: &str) {
    let lock = Arc::new(Mutex::new(()));
    EmitStdout(&format!("{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{}\"}}\n", EscapeJson(sessionId)), &lock);
    EmitStdout(&format!("{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n", EscapeJson(msg)), &lock);
    EmitStdout(&format!(
        "{{\"type\":\"result\",\"subtype\":\"error\",\"is_error\":true,\"result\":\"{}\",\"session_id\":\"{}\",\"usage\":{{\"input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":0}},\"modelUsage\":{{\"{}\":{{\"inputTokens\":0,\"outputTokens\":0,\"cacheReadInputTokens\":0,\"contextWindow\":200000,\"maxOutputTokens\":16384}}}}}}\n",
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
        let (rawM, rawE) = route.as_ref().map(|r| (r.model.as_str(), r.effort.as_str())).unwrap_or((cfg.model.as_str(), cfg.effort.as_str()));
        let model = match ResolveAgyModel(rawM, rawE) {
            Ok(m) => m,
            Err(err) => { eprintln!("{}", err); std::process::exit(1); }
        };
        let mut agyArgs = vec!["-p".to_string(), prompt.clone(), "--model".to_string(), model, "--disable-slash-commands".to_string()];
        ApplyModeArgs(&mut agyArgs, cfg.skipPermissions);
        let mut cmd = Command::new(ResolveAgyBin());
        cmd.args(&agyArgs);
        if !cfg.mcpConfig.trim().is_empty() {
            cmd.env("ROPILOT_SESSION_MCP_CONFIG", cfg.mcpConfig.trim());
        }
        match cmd.output() {
            Ok(o) => {
                let _ = io::stdout().write_all(&o.stdout);
                let _ = io::stderr().write_all(&o.stderr);
                std::process::exit(o.status.code().unwrap_or(0));
            }
            Err(err) => { eprintln!("Failed to run agy: {}", err); std::process::exit(1); }
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
        EmitStaticError(&cfg.sessionId, &cfg.model, "Real Claude Code CLI is not installed on this machine. Switch to the Gemini (AGY) tab or install Claude Code.");
        return;
    }

    let (rawM, rawE) = route.as_ref().map(|r| (r.model.as_str(), r.effort.as_str())).unwrap_or((cfg.model.as_str(), cfg.effort.as_str()));
    let target = match ResolveAgyModel(rawM, rawE) {
        Ok(m) => m,
        Err(err) => {
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
            EmitStaticError(&cfg.sessionId, &cfg.model, &err);
            std::process::exit(1);
        }
    };
    std::process::exit(RunSession(cfg, target));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ParsesJsonStringsAndSurrogates() {
        let (s, len) = ParseJsonString(r#""ok \uD83D\uDE00 \b\f""#).unwrap();
        assert_eq!(s, "ok \u{1F600} \u{0008}\u{000C}");
        assert_eq!(len, 22);
        assert!(ParseJsonString(r#""\u12""#).is_none());
    }

    #[test]
    fn ParsesCliFlagsSafely() {
        let args: Vec<String> = [
            "claude.exe", "-p", "--output-format", "stream-json",
            "--model", "claude-sonnet-5-5", "--session-id", "s1",
            "--system-prompt-file", "sys.txt", "stream-json prompt"
        ].iter().map(|s| s.to_string()).collect();
        let cfg = ParseCfg(&args);
        assert!(cfg.isStreamJson);
        assert!(cfg.printPrompt.is_none());
        assert_eq!(cfg.model, "claude-sonnet-5-5");
        assert_eq!(cfg.sessionId, "s1");
        assert_eq!(cfg.sysPromptFile, "sys.txt");
    }

    #[test]
    fn EncodesClaudeSlugAndSanitizesSessionId() {
        assert_eq!(EncodeClaudeSlug(r"C:\Users\my_user\proj.v1"), "C--Users-my_user-proj.v1");
        assert_eq!(SanitizeSessionId("../bad/id_1"), Some("badid_1".to_string()));
        assert_eq!(SanitizeSessionId("..//"), None);
    }

    #[test]
    fn HandlesToolErrorAndEmptyResult() {
        let mut st = TurnState {
            sessionId: "s1".into(),
            wireModel: "claude-sonnet-5-5".into(),
            modelName: "gemini-4-hydrogen-high".into(),
            convId: String::new(),
            savedConvId: String::new(),
            initEmitted: true,
            inFlight: true,
            textBuf: String::new(),
            emittedText: false,
            activeSteps: HashSet::new(),
            stderrTail: vec!["AGY_ERROR: boom".into()],
            transcriptLock: Arc::new(Mutex::new(None)),
            stdoutLock: Arc::new(Mutex::new(())),
        };
        HandleAgentLine(r#"{"step_type": "tool", "step_index": 3, "state": "ACTIVE", "tool_name": "Bash", "parameters": {"command": "echo 1"}}"#, &mut st);
        assert!(st.activeSteps.contains(&3));
        HandleAgentLine(r#"{"step_type": "tool", "step_index": 3, "state": "ERROR", "tool_info": {"error": {"message": "permission denied"}}}"#, &mut st);
        assert!(!st.activeSteps.contains(&3));
        let done = HandleAgentLine(r#"{"event": "result", "result": {"status": "SUCCESS", "response": ""}}"#, &mut st);
        assert!(done);
        assert!(!st.inFlight);
        assert!(st.stderrTail.is_empty());
    }
}