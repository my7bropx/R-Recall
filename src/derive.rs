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
}
