//! Working out which program an entry is about, from its commands.
//!
//! This is what lets the older, tool-less entries be found with `tool:nmap`
//! and displayed as `nmap › Host discovery`, and what tags newly ingested
//! commands automatically.

use crate::models::fenced_blocks_lang;

/// Fence languages whose contents are shell commands (or unlabelled, which in
/// practice means the same). Everything else — python, lua, regex, vim
/// keybindings, yaml… — is code or notation, not a program invocation.
pub fn is_shell_lang(lang: &str) -> bool {
    matches!(
        lang,
        "" | "bash" | "sh" | "shell" | "zsh" | "console" | "terminal" | "text" | "txt" | "cmd"
            | "powershell" | "ps1" | "pwsh" | "ssh" | "fish" | "dockerfile"
    )
}

/// Words that begin a line of code in common languages but are never a program.
const CODE_WORDS: &[&str] = &[
    "def", "class", "import", "from", "lambda", "print", "with", "try", "except", "finally",
    "raise", "assert", "pass", "yield", "async", "await", "global", "nonlocal", "del", "fn", "let",
    "const", "var", "int", "void", "char", "struct", "enum", "impl", "pub", "use", "package",
    "public", "private", "static", "func", "return",
];

/// Wrappers that precede the real program.
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "time", "nohup", "env", "nice", "ionice", "command", "builtin", "exec", "stdbuf",
    "proxychains", "proxychains4", "torify", "torsocks", "unbuffer",
];

/// Shell plumbing that says nothing about what the command is for.
const PLUMBING: &[&str] = &[
    "cd", "export", "unset", "set", "source", ".", "alias", "unalias", "pushd", "popd", "exit",
    "return", "clear", "true", "false", "read", "local", "declare", "typeset", "shopt", "trap",
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "{", "}", "[[", "]]", "[", "(", ")", "!", "wait", "test",
];

fn looks_like_program(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 40
        && t.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// The program a single command line runs, or `None` for comments, blank
/// lines, plumbing and things that are not commands.
pub fn tool_of_line(line: &str) -> Option<String> {
    let mut l = line.trim();
    if l.is_empty() || l.starts_with('#') {
        return None;
    }
    if let Some(rest) = l.strip_prefix("$ ") {
        l = rest.trim_start();
    }
    let toks: Vec<&str> = l.split_whitespace().collect();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if WRAPPERS.contains(&t) {
            i += 1;
            while i < toks.len() && toks[i].starts_with('-') {
                let takes_value = matches!(toks[i], "-u" | "-g" | "-n" | "-c");
                i += 1;
                if takes_value {
                    i += 1;
                }
            }
        } else if t.contains('=') && !t.starts_with('-') && !t.starts_with('/') && !t.starts_with('.')
            && t.split('=').next().is_some_and(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            i += 1; // VAR=value prefix
        } else {
            break;
        }
    }
    let first = toks.get(i)?;
    let base = first.rsplit('/').next().unwrap_or(first);
    let base = base.trim_end_matches([';', '&']);
    if !looks_like_program(base) {
        return None;
    }
    // Case-fold before the keyword checks: a prose line beginning "Use X
    // when:" inside a bare fence must not read as the plumbing word "use".
    let lower = base.to_lowercase();
    if PLUMBING.contains(&lower.as_str()) || CODE_WORDS.contains(&lower.as_str()) {
        return None;
    }
    // `secretsdump.py`, `linpeas.sh` → the name people actually say.
    let stem = ["py", "sh", "rb", "pl"]
        .iter()
        .find_map(|ext| lower.strip_suffix(&format!(".{}", ext)))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(lower);
    Some(stem)
}

/// The program of the first real command in a block of text.
pub fn tool_of_command(cmd: &str) -> String {
    cmd.lines().find_map(tool_of_line).unwrap_or_default()
}

/// Best guess at the tool an entry is about.
pub fn derive_tool(_title: &str, content: &str, tags: &[String]) -> String {
    // Keybinding-heavy topics: the topic *is* the tool, whatever the first line says.
    for t in tags {
        let l = t.to_lowercase();
        if l == "vim" || l == "tmux" {
            return l;
        }
    }
    for (lang, block) in fenced_blocks_lang(content) {
        if !is_shell_lang(&lang) {
            continue;
        }
        let t = tool_of_command(&block);
        if !t.is_empty() {
            return t;
        }
    }
    String::new()
}

// ─── vocabulary: which words at the start of a line really are programs ──────

/// Programs believed even in a file that never runs them inside a code block.
const SEED_PROGRAMS: &[&str] = &[
    "ls", "cd", "pwd", "cp", "mv", "rm", "mkdir", "rmdir", "touch", "cat", "less", "more", "head",
    "tail", "grep", "egrep", "fgrep", "rg", "sed", "awk", "cut", "sort", "uniq", "wc", "tr", "tee",
    "xargs", "find", "locate", "which", "whereis", "file", "stat", "ln", "chmod", "chown", "chgrp",
    "df", "du", "mount", "umount", "fdisk", "parted", "mkfs", "lsblk", "blkid", "dd", "ps", "top",
    "htop", "kill", "killall", "pkill", "systemctl", "journalctl", "service", "crontab", "date",
    "uptime", "who", "whoami", "id", "su", "sudo", "passwd", "useradd", "usermod", "userdel",
    "groupadd", "tar", "gzip", "gunzip", "zip", "unzip", "bzip2", "xz", "7z", "curl", "wget", "ssh",
    "scp", "sftp", "rsync", "nc", "ncat", "netcat", "nmap", "ping", "traceroute", "dig", "nslookup",
    "host", "whois", "ip", "ifconfig", "iwconfig", "ss", "netstat", "iptables", "ufw", "tcpdump",
    "tshark", "apt", "apt-get", "dpkg", "yum", "dnf", "pacman", "snap", "pip", "pip3", "npm", "yarn",
    "cargo", "rustc", "gcc", "make", "cmake", "python", "python3", "node", "ruby", "perl", "php",
    "java", "git", "docker", "kubectl", "vim", "nvim", "nano", "emacs", "tmux", "screen", "man",
    "echo", "printf", "env", "sleep", "clear", "history",
    "openssl", "gpg", "hashcat", "john", "hydra", "sqlmap", "nikto", "gobuster", "ffuf",
    "msfconsole", "msfvenom", "aircrack-ng", "airodump-ng", "aireplay-ng", "airmon-ng",
    "cryptsetup", "lsof", "strace", "ltrace", "objdump", "strings", "xxd", "hexdump", "base64",
    "md5sum", "sha1sum", "sha256sum", "diff", "patch", "watch", "timeout", "free", "vmstat",
    "iostat", "lsusb", "lspci", "lscpu", "dmesg", "uname", "hostname", "hostnamectl", "nmcli", "iw",
    "rfkill", "bluetoothctl", "fsck", "losetup", "modprobe", "lsmod", "chroot", "ssh-keygen",
    "ssh-copy-id", "jq", "tree", "column", "paste", "comm", "split", "shuf", "seq", "basename",
    "dirname", "realpath", "readlink", "mktemp", "install", "adduser", "chsh", "iptables-save",
    "nft", "arp", "route", "ethtool", "wpscan", "dirb", "enum4linux", "smbclient", "rpcclient",
    "ldapsearch", "searchsploit", "wireshark", "ffmpeg", "convert", "exiftool", "binwalk",
    "steghide", "foremost", "volatility", "vol.py", "gdb", "radare2", "r2", "flatpak",
    "gsettings", "xdg-open", "xclip", "wl-copy", "notify-send", "pgrep", "nice", "renice", "lsb_release",
];

/// English words that survive the program filters but are not software.
const NOT_PROGRAMS: &[&str] = &[
    "the", "and", "for", "you", "are", "this", "that", "with", "from", "your", "not", "can", "will",
    "then", "also", "note", "see", "when", "what", "how", "why", "use", "using", "just", "only",
    "all", "any", "one", "two", "each", "some", "step", "example", "output", "result", "usage",
    // heredoc terminators and block keywords are not programs
    "eof", "eot", "end", "done",
];

/// Common words that mark a line as a sentence rather than a command.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "to", "of", "and", "in", "it", "that", "this", "for", "on", "with",
    "you", "your", "be", "as", "or", "if", "then", "when", "can", "will", "by", "not",
];

/// Programs that appear in almost any snippet and so say little about its topic.
const HELPERS: &[&str] = &[
    "echo", "cat", "ls", "pwd", "sleep", "clear", "printf", "less", "more", "head", "tail",
];

/// Keybinding notation (`c-h`, `M-x`, `ctrl-b`, `ctrl+a`) reads like a program
/// to the line parser but never is one.
fn keybinding_like(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() == 3 && b[1] == b'-')
        || ["ctrl", "alt", "shift", "meta", "cmd", "super"]
            .iter()
            .any(|p| s.strip_prefix(p).is_some_and(|r| r.starts_with(['-', '+'])))
}

/// Every program a command line runs: one per pipeline / `&&` / `;` segment,
/// so `echo hi | sed s/h/j/` is about `sed`, not just `echo`. Separators inside
/// quotes and a trailing `# comment` are not separators.
pub fn programs_of_line(line: &str) -> Vec<String> {
    let mut segments: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut chars = line.char_indices().peekable();
    while let Some((pos, c)) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '#' if pos == 0 || line[..pos].ends_with(char::is_whitespace) => {
                    segments.push(&line[start..pos]);
                    start = line.len();
                    break;
                }
                '|' | ';' => {
                    segments.push(&line[start..pos]);
                    start = pos + 1;
                }
                '&' if chars.peek().is_some_and(|&(_, n)| n == '&') => {
                    segments.push(&line[start..pos]);
                    chars.next();
                    start = pos + 2;
                }
                _ => {}
            },
        }
    }
    if start < line.len() {
        segments.push(&line[start..]);
    }
    segments.into_iter().filter_map(tool_of_line).collect()
}

fn plausible_program(s: &str) -> bool {
    (2..=30).contains(&s.len())
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && !keybinding_like(s)
        && !NOT_PROGRAMS.contains(&s)
}

/// The set of programs a notes file is known to talk about: a built-in seed
/// plus every program that starts a line in at least three different shell
/// code blocks *of that file*. It is what lets the importer tell an unfenced
/// `nmap -sV host` (a command) from a sentence that merely starts with a word.
#[derive(Debug, Clone)]
pub struct Vocab {
    known: std::collections::HashSet<String>,
}

impl Vocab {
    pub fn seed() -> Self {
        Vocab { known: SEED_PROGRAMS.iter().map(|s| s.to_string()).collect() }
    }

    pub fn learn<'a>(shell_blocks: impl IntoIterator<Item = &'a str>) -> Self {
        let mut v = Vocab::seed();
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for block in shell_blocks {
            // A line that starts upper-case is a config directive or prose
            // (`HostbasedAuthentication no`), not a command: commands are lower-case.
            let stems: std::collections::HashSet<String> = block
                .lines()
                .filter(|l| !l.trim_start().starts_with(|c: char| c.is_ascii_uppercase()))
                .flat_map(programs_of_line)
                .filter(|s| plausible_program(s))
                .collect();
            for s in stems {
                *seen.entry(s).or_insert(0) += 1;
            }
        }
        v.known.extend(seen.into_iter().filter(|(_, n)| *n >= 3).map(|(s, _)| s));
        v
    }

    pub fn knows(&self, stem: &str) -> bool {
        self.known.contains(stem)
    }

    /// The program if `line` reads as a command invocation, else `None`.
    /// Deliberately strict — a wrong "yes" turns prose into a command:
    /// the first word must be lower-case (sentences start upper-case), the
    /// program must be a known one, a bare program name needs an argument (or a
    /// `$ ` prompt), and a line that reads like a sentence needs a shell marker
    /// (flag, path, pipe, redirect, quote, variable) to still count.
    pub fn command_line(&self, line: &str) -> Option<String> {
        let t = line.trim();
        let (t, prompted) = match t.strip_prefix("$ ") {
            Some(rest) => (rest.trim_start(), true),
            None => (t, false),
        };
        if t.is_empty() || t.starts_with('#') {
            return None;
        }
        let toks: Vec<&str> = t.split_whitespace().collect();
        if toks.len() < 2 && !prompted {
            return None;
        }
        if !prompted && !toks[0].starts_with(|c: char| c.is_ascii_lowercase() || matches!(c, '.' | '/' | '~')) {
            return None;
        }
        let stem = tool_of_line(t)?;
        if !self.known.contains(&stem) {
            return None;
        }
        if !prompted && !has_shell_marker(&toks) && prose_like(t, &toks) {
            return None;
        }
        Some(stem)
    }
}

fn has_shell_marker(toks: &[&str]) -> bool {
    toks[0] == "sudo"
        || toks.iter().skip(1).any(|t| {
            let flag = t.starts_with('-')
                && t.chars().nth(1).is_some_and(|c| c.is_ascii_alphabetic() || c == '-');
            flag || t.starts_with('/')
                || t.starts_with("./")
                || t.starts_with("~/")
                || t.starts_with('$')
                || t.starts_with('"')
                || t.starts_with('\'')
                || t.contains("://")
                || matches!(*t, "|" | ">" | ">>" | "<" | "&&" | "||" | ";" | "2>" | "2>&1")
        })
}

fn prose_like(t: &str, toks: &[&str]) -> bool {
    let stop = toks.iter().filter(|w| STOPWORDS.contains(&w.to_lowercase().as_str())).count();
    (toks.len() >= 3 && t.ends_with(['.', '?', '!'])) || (toks.len() >= 4 && stop >= 2)
}

/// The program an entry is most about, given the program of each of its
/// command lines: the most frequent one, provided it covers at least a third
/// of them (a grab-bag of unrelated commands has no single tool). Helpers like
/// `echo` and `cat` only win when nothing else is there; ties go to the
/// earliest.
pub fn dominant_tool(programs: &[String], vocab: &Vocab) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for p in programs.iter().filter(|p| vocab.knows(p)) {
        match counts.iter_mut().find(|(s, _)| *s == p.as_str()) {
            Some((_, n)) => *n += 1,
            None => counts.push((p.as_str(), 1)),
        }
    }
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    let pick = |skip_helpers: bool| {
        counts
            .iter()
            .filter(|(s, _)| !(skip_helpers && HELPERS.contains(s)))
            .fold(None::<(&str, usize)>, |best, &(s, n)| match best {
                Some((_, bn)) if bn >= n => best,
                _ => Some((s, n)),
            })
    };
    let (name, n) = pick(true).or_else(|| pick(false))?;
    (n * 3 >= total).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(content: &str, tags: &[&str]) -> String {
        derive_tool("t", content, &tags.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn takes_the_first_program_of_the_first_fenced_block() {
        assert_eq!(d("```bash\nuniq file.txt\n```", &[]), "uniq");
        assert_eq!(d("intro\n```\n# comment\nsudo nmap -sV x\n```", &[]), "nmap");
        assert_eq!(d("```\n$ ls -la\n```", &[]), "ls");
        assert_eq!(d("```\n/usr/bin/nmap x\n```", &[]), "nmap");
    }

    #[test]
    fn skips_wrappers_assignments_and_plumbing() {
        assert_eq!(d("```\nexport A=1\ncd /tmp\ncurl x\n```", &[]), "curl");
        assert_eq!(d("```\nFOO=bar ./run.sh\n```", &[]), "run", "assignment skipped, .sh dropped");
        assert_eq!(d("```\nsudo -u root -E env X=1 tcpdump -i eth0\n```", &[]), "tcpdump");
        assert_eq!(d("```\nproxychains4 nmap -sT x\n```", &[]), "nmap");
    }

    #[test]
    fn keybinding_topics_use_their_tag() {
        assert_eq!(d("```\ndd\n:wq\n```", &["linux", "vim"]), "vim");
        assert_eq!(d("```\nsudo apt install tmux\n```", &["tmux"]), "tmux");
    }

    #[test]
    fn code_fences_are_not_commands() {
        assert_eq!(d("```python\nclass Config:\n    x = 1\n```", &[]), "", "python fence skipped");
        assert_eq!(d("```python\nimport os\n```\n```bash\nls\n```", &[]), "ls", "later shell fence still counts");
        assert_eq!(d("```\nclass Foo:\n```", &[]), "", "keyword filter catches unlabelled code");
        assert_eq!(d("```lua\nrequire('x')\n```", &[]), "");
        assert_eq!(d("```regex\n^[a-z]+$\n```", &[]), "");
    }

    #[test]
    fn capitalized_prose_in_a_bare_fence_is_not_mistaken_for_a_program() {
        // real case: a bare ``` fence used for a plain-English comparison, not code.
        assert_eq!(d("```\nUse SSH when:\n- need tunneling\n```", &[]), "", "'Use' is prose, not a program");
        assert_eq!(d("```\nIf you need X\n```", &[]), "", "plumbing keyword, capitalized");
    }

    #[test]
    fn script_suffixes_are_dropped() {
        assert_eq!(d("```\npython3 vol.py -f x\n```", &[]), "python3");
        assert_eq!(d("```\n./linpeas.sh\n```", &[]), "linpeas");
        assert_eq!(d("```\nsecretsdump.py x\n```", &[]), "secretsdump");
    }

    #[test]
    fn nothing_to_go_on_gives_empty() {
        assert_eq!(d("just prose", &["linux"]), "");
        assert_eq!(d("```\n:wq\n{ }\n```", &[]), "");
        assert_eq!(d("", &[]), "");
    }

    #[test]
    fn tool_of_command_walks_lines_until_one_is_a_command() {
        assert_eq!(tool_of_command("# note\n\ncd /x\nnc -lvnp 4444"), "nc");
        assert_eq!(tool_of_command(""), "");
    }

    // ── vocabulary / unfenced command recognition ──

    fn seed() -> Vocab {
        Vocab::seed()
    }

    #[test]
    fn programs_of_line_sees_every_stage_of_a_pipeline() {
        assert_eq!(programs_of_line("echo hi | sed s/h/j/"), vec!["echo", "sed"]);
        assert_eq!(programs_of_line("cd /x && make -j4; sudo make install"), vec!["make", "make"], "cd is plumbing");
        assert_eq!(programs_of_line("grep -E 'a|b' file | sort"), vec!["grep", "sort"], "a | inside quotes is not a pipe");
        assert_eq!(programs_of_line("ls -la  # list; then | cat"), vec!["ls"], "a trailing comment is not code");
        assert_eq!(programs_of_line("# just a comment"), Vec::<String>::new());
        assert_eq!(programs_of_line("make || echo failed"), vec!["make", "echo"], "|| is a separator too");
    }

    #[test]
    fn command_line_needs_a_known_lowercase_program_and_an_argument() {
        let v = seed();
        assert_eq!(v.command_line("nmap -sV 10.0.0.1").as_deref(), Some("nmap"));
        assert_eq!(v.command_line("sudo apt update").as_deref(), Some("apt"));
        assert_eq!(v.command_line("$ ls").as_deref(), Some("ls"), "a prompt makes a bare program a command");
        assert_eq!(v.command_line("git status").as_deref(), Some("git"));
        assert_eq!(v.command_line("ls"), None, "a bare word alone is ambiguous");
        assert_eq!(v.command_line("Find the largest files"), None, "capitalised = a sentence");
        assert_eq!(v.command_line("find the largest files in the tree."), None, "reads as a sentence");
        assert_eq!(v.command_line("find . -size +100M").as_deref(), Some("find"), "flags rescue it");
        assert_eq!(v.command_line("# nmap -sV x"), None, "a comment");
        assert_eq!(v.command_line("frobnicate --all"), None, "unknown program");
    }

    #[test]
    fn a_program_seen_in_three_shell_blocks_becomes_known() {
        let blocks = ["hb-encrypt a", "hb-encrypt b\nls", "hb-encrypt c"];
        let v = Vocab::learn(blocks);
        assert!(v.knows("hb-encrypt"));
        let two = Vocab::learn(["zzz a", "zzz b"]);
        assert!(!two.knows("zzz"), "two blocks is not enough");
        let cfg = Vocab::learn(["HostbasedAuthentication no", "HostbasedAuthentication yes", "HostbasedAuthentication no"]);
        assert!(!cfg.knows("hostbasedauthentication"), "upper-case lines are config, not commands");
        let bindings = Vocab::learn(["C-h k", "C-h f", "C-h v", "Ctrl+a b", "ctrl+a c", "ctrl+a d", "EOF", "eof", "eof"]);
        assert!(!bindings.knows("c-h") && !bindings.knows("ctrl+a") && !bindings.knows("eof"));
    }

    #[test]
    fn dominant_tool_is_the_majority_program_helpers_last() {
        let v = seed();
        let p = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(dominant_tool(&p(&["ls", "docker", "docker", "docker"]), &v).as_deref(), Some("docker"));
        assert_eq!(dominant_tool(&p(&["echo", "sed", "echo", "sed"]), &v).as_deref(), Some("sed"), "helpers only win alone");
        assert_eq!(dominant_tool(&p(&["echo", "echo"]), &v).as_deref(), Some("echo"));
        assert_eq!(dominant_tool(&p(&["ls", "df", "free", "uname", "ip"]), &v), None, "no single tool");
        assert_eq!(dominant_tool(&p(&["frobnicate"]), &v), None, "unknown programs are not believed");
        assert_eq!(dominant_tool(&p(&["git", "curl"]), &v).as_deref(), Some("git"), "ties go to the earliest");
        assert_eq!(dominant_tool(&[], &v), None);
    }
}
