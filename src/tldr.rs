//! Opt-in ingestion from a local tldr-pages cache (tealdeer's, or the classic
//! Python client's). Never run automatically — a personal knowledge base
//! should not silently gain several thousand generic entries. `recall tldr
//! sync` is the only thing that calls this.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::db::{command_key, Database};
use crate::models::{Category, NewEntry, Source};

pub const DEFAULT_PLATFORMS: &[&str] = &["common", "linux"];
pub const ALL_PLATFORMS: &[&str] =
    &["common", "linux", "osx", "windows", "android", "sunos", "freebsd", "netbsd", "openbsd"];

#[derive(Debug, Clone, Default)]
pub struct TldrStats {
    pub pages: usize,
    pub candidates: usize,
    pub added: usize,
    pub duplicates: usize,
    pub already_covered: usize,
}

/// Every plausible cache root, existing or not — callers report the first
/// that exists, or list all of them in the "none found" error.
pub fn candidate_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("TLDR_CACHE_DIR") {
        out.push(PathBuf::from(p).join("pages.en"));
        out.push(PathBuf::from(std::env::var("TLDR_CACHE_DIR").unwrap()));
    }
    let xdg = std::env::var("XDG_CACHE_HOME").ok().map(PathBuf::from);
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    if let Some(x) = &xdg {
        out.push(x.join("tealdeer/tldr-pages/pages.en"));
        out.push(x.join("tldr/pages"));
        out.push(x.join("tldr-python-client"));
    }
    if let Some(h) = &home {
        out.push(h.join(".cache/tealdeer/tldr-pages/pages.en"));
        out.push(h.join(".cache/tldr/pages"));
        out.push(h.join(".cache/tldr-python-client"));
        out.push(h.join(".tldr/cache"));
    }
    out
}

/// The first candidate that actually looks like a tldr page tree (has at
/// least one known platform subdirectory).
pub fn find_cache() -> Option<PathBuf> {
    candidate_dirs().into_iter().find(|d| {
        ALL_PLATFORMS.iter().any(|p| d.join(p).is_dir())
    })
}

/// `{{[-a|--archive]}}` → `--archive` (last, usually the long form);
/// `{{state|exclude}}` → `state` (first, usually the representative one).
/// A genuine single-name placeholder (`{{path/to/file}}`, `{{port}}`) is left
/// with its braces so [`crate::fill`] still prompts for it.
fn simplify_placeholders(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut rest = cmd;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let Some(end_rel) = rest[start + 2..].find("}}") else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let inner = &rest[start + 2..start + 2 + end_rel];
        if inner.contains('|') {
            let bracketed = inner.starts_with('[') && inner.ends_with(']');
            let body = if bracketed { &inner[1..inner.len() - 1] } else { inner };
            let alts: Vec<&str> = body.split('|').map(|s| s.trim()).collect();
            let chosen = if bracketed { alts.last() } else { alts.first() };
            if let Some(c) = chosen {
                out.push_str(c);
            }
        } else {
            out.push_str("{{");
            out.push_str(inner);
            out.push_str("}}");
        }
        rest = &rest[start + 2 + end_rel + 2..];
    }
    out.push_str(rest);
    out
}

struct Page {
    tool: String,
    examples: Vec<(String, String)>, // (description, command)
}

fn parse_page(text: &str) -> Option<Page> {
    let mut lines = text.lines();
    let tool = lines.next()?.trim_start_matches('#').trim().to_lowercase();
    if tool.is_empty() {
        return None;
    }
    let mut examples = Vec::new();
    let mut desc: Option<String> = None;
    for line in lines {
        let t = line.trim();
        if let Some(d) = t.strip_prefix("- ") {
            let d = d.trim().trim_end_matches(':').trim();
            desc = Some(d.to_string());
        } else if t.starts_with('`') && t.ends_with('`') && t.len() >= 2 {
            if let Some(d) = desc.take() {
                let cmd = t.trim_matches('`');
                examples.push((d, simplify_placeholders(cmd)));
            }
        }
    }
    Some(Page { tool, examples })
}

/// Ingest the given platforms (in priority order — a page seen under `common`
/// is not re-added from `linux`) from `cache_dir`. Set `only_tool` to import
/// a single command's page.
pub fn ingest(
    db: &mut Database,
    cache_dir: &Path,
    platforms: &[String],
    only_tool: Option<&str>,
) -> Result<TldrStats> {
    if !cache_dir.is_dir() {
        bail!("{} is not a directory", cache_dir.display());
    }
    let mut stats = TldrStats::default();
    let mut seen_tools: HashSet<String> = HashSet::new();
    let existing_tc = db.existing_title_content()?;
    let existing_cmd = db.existing_command_keys()?;
    let mut batch: Vec<NewEntry> = Vec::new();
    let mut seen_this_run: HashSet<(String, String)> = HashSet::new();

    for platform in platforms {
        let dir = cache_dir.join(platform);
        if !dir.is_dir() {
            continue;
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "md").unwrap_or(false))
            .collect();
        files.sort();

        for path in files {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if let Some(only) = only_tool {
                if stem != only.to_lowercase() {
                    continue;
                }
            }
            if !seen_tools.insert(stem.clone()) {
                continue; // a more specific platform already supplied this tool
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Some(page) = parse_page(&text) else { continue };
            stats.pages += 1;

            for (desc, cmd) in page.examples {
                stats.candidates += 1;
                let title = desc;
                let key_tc = (title.clone(), cmd.clone());
                if existing_tc.contains(&key_tc) || !seen_this_run.insert(key_tc) {
                    stats.duplicates += 1;
                    continue;
                }
                if existing_cmd.contains(&command_key(&page.tool, &cmd)) {
                    stats.already_covered += 1;
                    continue;
                }
                let danger = crate::danger::danger_reason(&cmd).is_some();
                batch.push(NewEntry {
                    title,
                    content: String::new(),
                    category: Category::Command,
                    tags: vec!["tldr".into(), platform.clone()],
                    tool: page.tool.clone(),
                    command: cmd,
                    keywords: String::new(),
                    danger,
                    source: Source::Tldr,
                    pack_key: String::new(),
                });
            }
        }
    }

    stats.added = batch.len();
    if !batch.is_empty() {
        db.add_entries(&batch)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_db() -> (Database, PathBuf) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("recall_tldrtest_{}_{}.db", std::process::id(), nanos));
        let _ = std::fs::remove_file(&p);
        (Database::new(&p).unwrap(), p)
    }

    fn cleanup(p: &Path) {
        for ext in ["", "-wal", "-shm"] {
            let mut s = p.as_os_str().to_owned();
            s.push(ext);
            let _ = std::fs::remove_file(PathBuf::from(s));
        }
    }

    #[test]
    fn simplifies_flag_alternatives_but_keeps_real_placeholders() {
        assert_eq!(
            simplify_placeholders("rsync {{[-a|--archive]}} {{path/to/source}} {{path/to/destination}}"),
            "rsync --archive {{path/to/source}} {{path/to/destination}}"
        );
        assert_eq!(simplify_placeholders("ss {{state|exclude}} {{bucket|big|connected}}"), "ss state bucket");
        assert_eq!(simplify_placeholders("tar {{[-z|--gzip]}} -cf {{out}}"), "tar --gzip -cf {{out}}");
        assert_eq!(simplify_placeholders("plain, no placeholders"), "plain, no placeholders");
    }

    #[test]
    fn parses_a_real_tldr_style_page() {
        let text = "# rsync\n\n> Transfer files.\n> More info: <url>.\n\n- Transfer a file (simulate with `--dry-run`):\n\n`rsync {{path/to/source}} {{path/to/destination}}`\n\n- Archive mode:\n\n`rsync {{[-a|--archive]}} {{path/to/source}} {{path/to/destination}}`\n";
        let page = parse_page(text).unwrap();
        assert_eq!(page.tool, "rsync");
        assert_eq!(page.examples.len(), 2);
        assert_eq!(page.examples[0].0, "Transfer a file (simulate with `--dry-run`)");
        assert_eq!(page.examples[0].1, "rsync {{path/to/source}} {{path/to/destination}}");
        assert_eq!(page.examples[1].1, "rsync --archive {{path/to/source}} {{path/to/destination}}");
    }

    fn write_page(dir: &Path, platform: &str, name: &str, text: &str) {
        std::fs::create_dir_all(dir.join(platform)).unwrap();
        std::fs::write(dir.join(platform).join(format!("{}.md", name)), text).unwrap();
    }

    #[test]
    fn ingest_dedupes_against_existing_rows_and_skips_covered_commands() {
        let (mut db, dbp) = tmp_db();
        db.add_new(&NewEntry {
            title: "x".into(),
            tool: "nmap".into(),
            command: "nmap -p- {{target}}".into(),
            category: Category::Command,
            ..Default::default()
        })
        .unwrap();

        let mut cache = std::env::temp_dir();
        cache.push(format!("recall_tldr_cache_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        write_page(
            &cache,
            "common",
            "nmap",
            "# nmap\n\n- Full port sweep:\n\n`nmap -p- {{target}}`\n\n- Ping sweep:\n\n`nmap -sn {{subnet}}`\n",
        );
        write_page(&cache, "common", "jq", "# jq\n\n- Pretty print:\n\n`jq {{'.'}}`\n");

        let platforms = vec!["common".to_string()];
        let s = ingest(&mut db, &cache, &platforms, None).unwrap();
        assert_eq!(s.pages, 2);
        assert_eq!(s.candidates, 3);
        assert_eq!(s.already_covered, 1, "nmap -p- <target> already exists under a different title");
        assert_eq!(s.added, 2, "ping sweep + jq are new");

        let all = db.get_all_entries().unwrap();
        assert!(all.iter().any(|e| e.tool == "jq" && e.source == Source::Tldr));
        assert!(all.iter().all(|e| e.tool != "jq" || e.tags.contains(&"tldr".to_string())));

        // running again adds nothing new
        let s2 = ingest(&mut db, &cache, &platforms, None).unwrap();
        assert_eq!(s2.added, 0);

        drop(db);
        cleanup(&dbp);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn only_tool_filters_to_one_page() {
        let (mut db, dbp) = tmp_db();
        let mut cache = std::env::temp_dir();
        cache.push(format!("recall_tldr_cache2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        write_page(&cache, "common", "jq", "# jq\n\n- A:\n\n`jq .`\n");
        write_page(&cache, "common", "yq", "# yq\n\n- A:\n\n`yq .`\n");
        let platforms = vec!["common".to_string()];
        let s = ingest(&mut db, &cache, &platforms, Some("jq")).unwrap();
        assert_eq!(s.pages, 1);
        assert_eq!(s.added, 1);
        drop(db);
        cleanup(&dbp);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_platform_seen_earlier_wins_over_a_later_duplicate() {
        let (mut db, dbp) = tmp_db();
        let mut cache = std::env::temp_dir();
        cache.push(format!("recall_tldr_cache3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        write_page(&cache, "common", "ls", "# ls\n\n- Common version:\n\n`ls -la`\n");
        write_page(&cache, "linux", "ls", "# ls\n\n- Linux-only version:\n\n`ls --color`\n");
        let platforms = vec!["common".to_string(), "linux".to_string()];
        let s = ingest(&mut db, &cache, &platforms, None).unwrap();
        assert_eq!(s.pages, 1, "linux/ls.md is shadowed by common/ls.md");
        let all = db.get_all_entries().unwrap();
        assert_eq!(all[0].command, "ls -la");
        drop(db);
        cleanup(&dbp);
        let _ = std::fs::remove_dir_all(&cache);
    }
}
