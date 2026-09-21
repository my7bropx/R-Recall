//! Placeholders and the variables that fill them.
//!
//! Commands are templates. Two spellings are recognised:
//!
//! * `{{name}}` / `{{name:default}}` — used by the built-in pack and tldr;
//! * `<name>` — the style found in hand-written notes (`nmap -p- <target>`).
//!
//! Vim key notation (`<Space>`, `<C-w>`), HTML tags (`<script>`) and Go/Jinja
//! templates (`{{.Names}}`, `{{ item }}`) are deliberately *not* placeholders.
//!
//! Values come from, in order: explicit answers → variables you pinned with
//! `recall set` → a few safe built-ins (`lhost`, `date`, …) → inline defaults.
//! Related names share one variable, so pinning `target` also fills `<ip>`,
//! `<host>` and `{{rhost}}`.

use std::collections::HashMap;
use std::process::Command;

// ─── canonical names ─────────────────────────────────────────────────────────

/// canonical key → spellings that mean the same thing
const ALIASES: &[(&str, &[&str])] = &[
    ("target", &[
        "ip", "ips", "host", "hosts", "rhost", "rhosts", "targetip", "target_ip", "target_host",
        "victim", "address", "ip_address", "ipaddr", "ipaddress", "remote_host", "dst",
    ]),
    ("lhost", &["attacker", "attacker_ip", "myip", "my_ip", "local_ip", "localip", "local_host"]),
    ("lport", &["listen_port", "local_port", "myport"]),
    ("rport", &["remote_port"]),
    ("domain", &["dom", "domain_name", "realm"]),
    ("user", &["username", "login", "uname", "account"]),
    ("pass", &["password", "passwd", "pw"]),
    ("url", &["uri", "site", "website"]),
    ("iface", &["interface", "nic", "net_iface"]),
    ("wordlist", &["wl", "dict", "dictionary", "wordfile"]),
];

/// Lower-cased, `-` → `_`, and aliases folded onto their canonical key.
pub fn canon(name: &str) -> String {
    let n = name.trim().to_lowercase().replace('-', "_");
    for (key, alts) in ALIASES {
        if n == *key || alts.contains(&n.as_str()) {
            return (*key).to_string();
        }
    }
    n
}

// ─── scanning ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Ph {
    /// As written, used as the prompt label (`target`, `Wordlist`).
    pub label: String,
    /// Canonical variable key (`target`).
    pub key: String,
    /// `{{name:default}}`'s default, if any.
    pub default: Option<String>,
    /// Byte range of the whole placeholder in the source string.
    pub start: usize,
    pub end: usize,
}

const TEMPLATE_WORDS: &[&str] = &[
    "end", "else", "range", "if", "with", "define", "template", "block", "break", "continue",
    "nil", "true", "false", "elif", "endif", "endfor", "for",
];

const HTML_TAGS: &[&str] = &[
    "a", "abbr", "address", "applet", "article", "audio", "b", "base", "body", "br", "button",
    "canvas", "center", "code", "details", "div", "em", "embed", "font", "footer", "form", "frame",
    "frameset", "h1", "h2", "h3", "h4", "h5", "h6", "head", "header", "hr", "html", "i", "iframe",
    "img", "input", "label", "li", "link", "marquee", "meta", "nav", "object", "ol", "option", "p",
    "pre", "script", "section", "select", "source", "span", "strong", "style", "summary", "svg",
    "table", "td", "textarea", "th", "title", "tr", "u", "ul", "video", "xml",
];

const VIM_KEYS: &[&str] = &[
    "space", "tab", "cr", "esc", "enter", "return", "bs", "del", "up", "down", "left", "right",
    "home", "end", "pageup", "pagedown", "insert", "leader", "localleader", "plug", "nop", "cmd",
    "bar", "lt", "gt", "silent", "buffer", "expr", "nowait", "bslash", "eol", "nul", "lf", "ff",
];

fn is_vim_key(inner: &str) -> bool {
    let l = inner.to_lowercase();
    if VIM_KEYS.contains(&l.as_str()) {
        return true;
    }
    let b = l.as_bytes();
    // <C-w> <S-Tab> <A-x> <M-x> <D-x>
    if b.len() >= 3 && b[1] == b'-' && matches!(b[0], b'c' | b's' | b'a' | b'm' | b'd') {
        return true;
    }
    // <F1>…<F12>, <k0>…
    if (b[0] == b'f' || b[0] == b'k') && b.len() > 1 && b[1..].iter().all(|c| c.is_ascii_digit()) {
        return true;
    }
    false
}

fn valid_curly_name(n: &str) -> bool {
    if n.is_empty() || n.len() > 48 {
        return false;
    }
    let mut ch = n.chars();
    let first = ch.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    if !n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')) {
        return false;
    }
    !TEMPLATE_WORDS.contains(&n.to_lowercase().as_str())
}

fn parse_curly(inner: &str) -> Option<(String, Option<String>)> {
    let (name, def) = match inner.find(':') {
        Some(i) => (&inner[..i], Some(&inner[i + 1..])),
        None => (inner, None),
    };
    if !valid_curly_name(name) {
        return None;
    }
    let default = def
        .filter(|d| !d.is_empty() && d.len() <= 200 && !d.contains('\n'))
        .map(|d| d.to_string());
    Some((name.to_string(), default))
}

fn parse_angle(inner: &str) -> Option<String> {
    if inner.is_empty() || inner.len() > 30 {
        return None;
    }
    let mut ch = inner.chars();
    if !ch.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    if !inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    let l = inner.to_lowercase();
    if HTML_TAGS.contains(&l.as_str()) || is_vim_key(inner) {
        return None;
    }
    Some(inner.to_string())
}

/// Every placeholder occurrence in `cmd`, in order.
pub fn scan(cmd: &str) -> Vec<Ph> {
    let b = cmd.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'{' && i + 1 < b.len() && b[i + 1] == b'{' {
            if let Some(rel) = cmd[i + 2..].find("}}") {
                let inner = &cmd[i + 2..i + 2 + rel];
                if let Some((label, default)) = parse_curly(inner) {
                    let end = i + 2 + rel + 2;
                    out.push(Ph { key: canon(&label), label, default, start: i, end });
                    i = end;
                    continue;
                }
            }
            i += 2;
            continue;
        }
        if b[i] == b'<' {
            let window_end = (i + 32).min(b.len());
            if let Some(rel) = cmd[i + 1..window_end].find('>') {
                let inner = &cmd[i + 1..i + 1 + rel];
                if let Some(label) = parse_angle(inner) {
                    let end = i + 1 + rel + 1;
                    out.push(Ph { key: canon(&label), label, default: None, start: i, end });
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Placeholders de-duplicated by canonical key (first occurrence wins, but a
/// later occurrence may contribute the default).
pub fn unique(cmd: &str) -> Vec<Ph> {
    let mut out: Vec<Ph> = Vec::new();
    for p in scan(cmd) {
        match out.iter_mut().find(|q| q.key == p.key) {
            Some(q) => {
                if q.default.is_none() {
                    q.default = p.default;
                }
            }
            None => out.push(p),
        }
    }
    out
}

/// Substitute every placeholder whose key is in `values`; leave the rest as written.
pub fn apply(cmd: &str, values: &HashMap<String, String>) -> String {
    let phs = scan(cmd);
    if phs.is_empty() || values.is_empty() {
        return cmd.to_string();
    }
    let mut out = String::with_capacity(cmd.len());
    let mut last = 0usize;
    for p in &phs {
        if let Some(v) = values.get(&p.key) {
            out.push_str(&cmd[last..p.start]);
            out.push_str(v);
            last = p.end;
        }
    }
    out.push_str(&cmd[last..]);
    out
}

// ─── resolution ──────────────────────────────────────────────────────────────

/// Variables available when filling: what the user pinned, and the last value
/// typed for each name (used only to pre-fill prompts).
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub pinned: HashMap<String, String>,
    pub last: HashMap<String, String>,
}

/// A placeholder nobody has supplied a value for yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub label: String,
    pub key: String,
    /// What to offer / fall back to: last typed value, inline default, or a
    /// detected value such as the default-route interface.
    pub suggest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    /// The command with everything fillable already substituted.
    pub text: String,
    /// What is still open.
    pub pending: Vec<Pending>,
}

/// First pass: substitute everything that can be filled without asking.
pub fn resolve(cmd: &str, explicit: &HashMap<String, String>, ctx: &Ctx) -> Resolved {
    let mut known: HashMap<String, String> = HashMap::new();
    let mut pending: Vec<Pending> = Vec::new();
    for p in unique(cmd) {
        let v = explicit
            .get(&p.key)
            .or_else(|| ctx.pinned.get(&p.key))
            .cloned()
            .or_else(|| silent_builtin(&p.key));
        match v {
            Some(v) => {
                known.insert(p.key.clone(), v);
            }
            None => {
                let suggest = ctx
                    .last
                    .get(&p.key)
                    .cloned()
                    .or(p.default.clone())
                    .or_else(|| suggest_builtin(&p.key));
                pending.push(Pending { label: p.label, key: p.key, suggest });
            }
        }
    }
    Resolved { text: apply(cmd, &known), pending }
}

/// Fill without prompting: pending placeholders take their suggestion when
/// there is one. Returns the text and the labels that stayed unresolved.
pub fn fill_quiet(cmd: &str, explicit: &HashMap<String, String>, ctx: &Ctx) -> (String, Vec<String>) {
    let r = resolve(cmd, explicit, ctx);
    let mut answers: HashMap<String, String> = HashMap::new();
    let mut unresolved = Vec::new();
    for p in &r.pending {
        match &p.suggest {
            Some(s) => {
                answers.insert(p.key.clone(), s.clone());
            }
            None => unresolved.push(p.label.clone()),
        }
    }
    (apply(&r.text, &answers), unresolved)
}

/// Apply typed answers to what [`resolve`] left open. An empty answer accepts
/// the suggestion; with no suggestion the placeholder stays as written.
pub fn finish(text: &str, pending: &[Pending], typed: &HashMap<String, String>) -> String {
    let mut answers: HashMap<String, String> = HashMap::new();
    for p in pending {
        let t = typed.get(&p.key).map(|s| s.as_str()).unwrap_or("");
        if !t.is_empty() {
            answers.insert(p.key.clone(), t.to_string());
        } else if let Some(s) = &p.suggest {
            answers.insert(p.key.clone(), s.clone());
        }
    }
    apply(text, &answers)
}

// ─── built-in variables ──────────────────────────────────────────────────────

/// Built-ins that are safe to substitute without asking.
pub fn silent_builtin(key: &str) -> Option<String> {
    let now = chrono::Local::now();
    match key {
        "date" => Some(now.format("%Y-%m-%d").to_string()),
        "time" => Some(now.format("%H:%M:%S").to_string()),
        "timestamp" => Some(now.format("%Y%m%d-%H%M%S").to_string()),
        "cwd" => std::env::current_dir().ok().map(|p| p.display().to_string()),
        "home" => std::env::var("HOME").ok(),
        "lhost" => detect_lhost(),
        "subnet" => detect_subnet(),
        _ => None,
    }
}

/// Built-ins that are only offered as a suggestion (they could be wrong for the
/// task at hand, e.g. bringing the wrong interface down).
pub fn suggest_builtin(key: &str) -> Option<String> {
    match key {
        "iface" => default_iface(),
        _ => None,
    }
}

/// Names that [`silent_builtin`] / [`suggest_builtin`] can supply, for `recall vars`.
pub const BUILTIN_NAMES: &[&str] =
    &["lhost", "subnet", "iface", "date", "time", "timestamp", "cwd", "home"];

pub fn parse_default_route(proc_net_route: &str) -> Option<String> {
    for line in proc_net_route.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 4 && f[1] == "00000000" {
            let flags = u32::from_str_radix(f[3], 16).unwrap_or(0);
            if flags & 1 == 1 {
                return Some(f[0].to_string());
            }
        }
    }
    None
}

pub fn default_iface() -> Option<String> {
    let t = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_default_route(&t)
}

/// `inet 10.10.14.5/23 …` → (10.10.14.5, 23)
pub fn parse_inet(ip_addr_output: &str) -> Option<(std::net::Ipv4Addr, u8)> {
    for tok in ip_addr_output.split_whitespace().collect::<Vec<_>>().windows(2) {
        if tok[0] == "inet" {
            let (addr, pfx) = tok[1].split_once('/')?;
            return Some((addr.parse().ok()?, pfx.parse().ok()?));
        }
    }
    None
}

pub fn network_of(addr: std::net::Ipv4Addr, prefix: u8) -> String {
    let p = prefix.min(32) as u32;
    let mask: u32 = if p == 0 { 0 } else { u32::MAX << (32 - p) };
    let net = std::net::Ipv4Addr::from(u32::from(addr) & mask);
    format!("{}/{}", net, p)
}

fn iface_ipv4(iface: &str) -> Option<(std::net::Ipv4Addr, u8)> {
    let out = Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", iface])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_inet(&String::from_utf8_lossy(&out.stdout))
}

/// Your address to listen on: the VPN interface if there is one (HTB/THM
/// style `tun0`), otherwise the default-route interface.
pub fn detect_lhost() -> Option<String> {
    for name in ["tun0", "tun1", "tap0", "wg0"] {
        if std::path::Path::new("/sys/class/net").join(name).exists() {
            if let Some((a, _)) = iface_ipv4(name) {
                return Some(a.to_string());
            }
        }
    }
    let i = default_iface()?;
    iface_ipv4(&i).map(|(a, _)| a.to_string())
}

pub fn detect_subnet() -> Option<String> {
    let i = default_iface()?;
    iface_ipv4(&i).map(|(a, p)| network_of(a, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(cmd: &str) -> Vec<String> {
        scan(cmd).into_iter().map(|p| p.label).collect()
    }

    #[test]
    fn curly_and_angle_placeholders_are_found_in_order() {
        assert_eq!(labels("nmap -p- {{target}} -oA {{out:scan}} <ip>"), vec!["target", "out", "ip"]);
    }

    #[test]
    fn defaults_are_parsed() {
        let p = &scan("gobuster dir -w {{wordlist:/usr/share/wordlists/dirb/common.txt}}")[0];
        assert_eq!(p.label, "wordlist");
        assert_eq!(p.default.as_deref(), Some("/usr/share/wordlists/dirb/common.txt"));
        let u = &scan("curl {{url:http://x/y}}")[0];
        assert_eq!(u.default.as_deref(), Some("http://x/y"), "only the first colon splits");
    }

    #[test]
    fn vim_keys_html_and_templates_are_not_placeholders() {
        assert!(scan("nnoremap <Space>w :w<CR> <C-w>h <leader>x <F5> <S-Tab>").is_empty());
        assert!(scan("<script>alert(1)</script><img src=x>").is_empty());
        assert!(scan("docker ps --format '{{.Names}}' -f '{{ .State }}' {{end}} {{range .x}}").is_empty());
        assert!(scan("ansible -m debug -a 'msg={{ item }}'").is_empty(), "spaces inside braces");
        assert!(scan("cat <<EOF\nx\nEOF").is_empty(), "heredoc");
        assert!(scan("a<b and 2>&1 and x -> y").is_empty());
    }

    #[test]
    fn angle_placeholders_from_real_notes_work() {
        assert_eq!(labels("git checkout <branch> && git log <commit>"), vec!["branch", "commit"]);
        assert_eq!(labels("ssh <username>@<host>"), vec!["username", "host"]);
        assert_eq!(labels("curl http://<target>/x"), vec!["target"]);
    }

    #[test]
    fn aliases_share_one_key() {
        assert_eq!(canon("IP"), "target");
        assert_eq!(canon("rhosts"), "target");
        assert_eq!(canon("Username"), "user");
        assert_eq!(canon("target-ip"), "target");
        assert_eq!(canon("file"), "file");
        let u = unique("nmap <target> {{ip}} {{host}}");
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn apply_replaces_only_known_keys() {
        let mut v = HashMap::new();
        v.insert("target".to_string(), "10.0.0.5".to_string());
        assert_eq!(apply("nmap {{target}} <ip> {{out}}", &v), "nmap 10.0.0.5 10.0.0.5 {{out}}");
        assert_eq!(apply("no placeholders", &v), "no placeholders");
    }

    #[test]
    fn resolve_uses_explicit_then_pinned_then_defaults() {
        let mut ctx = Ctx::default();
        ctx.pinned.insert("target".into(), "10.1.1.1".into());
        ctx.last.insert("file".into(), "old.txt".into());
        let mut explicit = HashMap::new();
        explicit.insert("port".to_string(), "8080".to_string());

        let r = resolve("x {{target}} {{port:80}} {{file}} {{out:res}}", &explicit, &ctx);
        assert_eq!(r.text, "x 10.1.1.1 8080 {{file}} {{out:res}}");
        assert_eq!(r.pending.len(), 2);
        assert_eq!(r.pending[0].suggest.as_deref(), Some("old.txt"), "last typed value pre-fills");
        assert_eq!(r.pending[1].suggest.as_deref(), Some("res"), "inline default pre-fills");
    }

    #[test]
    fn fill_quiet_takes_suggestions_and_reports_the_rest() {
        let (t, un) = fill_quiet("a {{x:1}} {{y}}", &HashMap::new(), &Ctx::default());
        assert_eq!(t, "a 1 {{y}}");
        assert_eq!(un, vec!["y"]);
    }

    #[test]
    fn finish_accepts_typed_values_or_falls_back() {
        let r = resolve("a {{x:1}} {{y}} {{z:9}}", &HashMap::new(), &Ctx::default());
        let mut typed = HashMap::new();
        typed.insert("x".to_string(), "42".to_string());
        typed.insert("z".to_string(), String::new()); // empty → accept the suggestion
        assert_eq!(finish(&r.text, &r.pending, &typed), "a 42 {{y}} 9");
    }

    #[test]
    fn unicode_around_placeholders_is_safe() {
        let (t, _) = fill_quiet("echo «{{x:é}}» → <name>", &HashMap::new(), &Ctx::default());
        assert_eq!(t, "echo «é» → <name>");
    }

    #[test]
    fn route_and_inet_parsing() {
        let route = "Iface\tDestination\tGateway\tFlags\nwlan0\t00000000\t0100A8C0\t0003\t0\t0\t600\n";
        assert_eq!(parse_default_route(route).as_deref(), Some("wlan0"));
        assert_eq!(parse_default_route("Iface\tDestination\tGateway\tFlags\n"), None);

        let (a, p) = parse_inet("2: tun0    inet 10.10.14.5/23 brd 10.10.15.255 scope global tun0").unwrap();
        assert_eq!((a.to_string(), p), ("10.10.14.5".to_string(), 23));
        assert_eq!(network_of(a, p), "10.10.14.0/23");
        assert_eq!(network_of("192.168.1.77".parse().unwrap(), 24), "192.168.1.0/24");
        assert_eq!(network_of("8.8.8.8".parse().unwrap(), 0), "0.0.0.0/0");
    }
}
