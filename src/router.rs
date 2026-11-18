#![allow(non_snake_case)]
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct McpTarget {
    cmd: String,
    args: Vec<String>,
    env: HashMap<String, String>,
}

/// Resolves Ropilot Studio's per-user data directory across Windows, macOS, and Linux.
/// @return Path to the `ropilot` userData directory.
fn UserDataDir() -> PathBuf {
    #[cfg(windows)]
    {
        return PathBuf::from(env::var("APPDATA").unwrap_or_default()).join("ropilot");
    }
    #[cfg(target_os = "macos")]
    {
        let home = env::var("HOME").unwrap_or_default();
        return PathBuf::from(home).join("Library").join("Application Support").join("ropilot");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let cfg = env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| PathBuf::from(env::var("HOME").unwrap_or_default()).join(".config").to_string_lossy().to_string());
        return PathBuf::from(cfg).join("ropilot");
    }
}

/// Decodes one JSON string literal starting at the opening quote.
/// @param slice Slice beginning with `"`.
/// @return Decoded string and consumed byte length, or `None` if malformed.
fn ParseString(slice: &str) -> Option<(String, usize)> {
    let mut chars = slice.char_indices();
    if chars.next()?.1 != '"' {
        return None;
    }
    let mut out = String::new();
    while let Some((idx, ch)) = chars.next() {
        if ch == '"' {
            return Some((out, idx + 1));
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let (_, esc) = chars.next()?;
        match esc {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let mut hex = String::new();
                for _ in 0..4 {
                    if let Some((_, h)) = chars.next() {
                        hex.push(h);
                    }
                }
                if let Some(cp) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(cp);
                }
            }
            other => out.push(other),
        }
    }
    None
}

/// Extracts a balanced `{...}` or `[...]` block while ignoring delimiters inside strings.
/// @param text Source JSON text.
/// @param open Opening delimiter.
/// @param close Closing delimiter.
/// @return Subslice covering the balanced block.
fn ExtractBlock(text: &str, open: char, close: char) -> Option<&str> {
    let start = text.find(open)?;
    let slice = &text[start..];
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;

    for (idx, ch) in slice.char_indices() {
        if escaped { escaped = false; continue; }
        if ch == '\\' && quoted { escaped = true; continue; }
        if ch == '"' { quoted = !quoted; continue; }
        if quoted { continue; }
        if ch == open { depth += 1; }
        else if ch == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(&slice[..=idx]);
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
                    pos += 1;
                    continue;
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

/// Reads a JSON string array field (`args`) from an object slice.
/// @param obj Enclosing JSON object slice.
/// @param key Field name holding the string array.
/// @return Decoded strings in order.
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

/// Reads a string-to-string map (`env`) from an object slice.
/// @param obj Enclosing JSON object slice.
/// @return Environment variable map.
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
        if let Some((v, vLen)) = ParseString(trimmed) {
            map.insert(key, v);
            pos += (trimmed.as_ptr() as usize - rest.as_ptr() as usize) + vLen;
        } else {
            pos += kOff + kLen;
        }
    }
    map
}

/// Locates the active Ropilot session MCP config file from env or marker file.
/// @return Path string when a session config file exists on disk.
fn ResolveConfigPath() -> Option<String> {
    if let Ok(p) = env::var("ROPILOT_SESSION_MCP_CONFIG") {
        let s = p.trim().to_string();
        if !s.is_empty() && Path::new(&s).is_file() {
            return Some(s);
        }
    }
    let marker = fs::read_to_string(UserDataDir().join("agy-session-mcp.txt")).ok()?;
    let s = marker.trim().to_string();
    if !s.is_empty() && Path::new(&s).is_file() { Some(s) } else { None }
}

/// Loads the live `ropilot` MCP server command, args, and env for this session.
/// @return Parsed `McpTarget` when a valid session config is active.
fn LoadSessionTarget() -> Option<McpTarget> {
    let cfgPath = ResolveConfigPath()?;
    let raw = fs::read_to_string(cfgPath).ok()?;
    let servers = ExtractBlock(FindField(&raw, "mcpServers")?, '{', '}')?;
    let ropilot = ExtractBlock(FindField(servers, "ropilot")?, '{', '}')?;
    let (cmd, _) = ParseString(FindField(ropilot, "command")?)?;
    Some(McpTarget {
        cmd,
        args: ReadStringArray(ropilot, "args"),
        env: ReadEnvMap(ropilot),
    })
}

/// Builds the standalone fallback target when launched outside Ropilot Studio.
/// @return Fallback `McpTarget` pointing at `ropilot-infra-helper`.
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

fn main() {
    let target = LoadSessionTarget().unwrap_or_else(DefaultTarget);
    let status = Command::new(&target.cmd)
        .args(&target.args)
        .envs(&target.env)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(0)),
        Err(err) => {
            eprintln!("ropilot-mcp-router failed to spawn {}: {}", target.cmd, err);
            std::process::exit(1);
        }
    }
}