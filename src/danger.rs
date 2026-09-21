//! Heuristics for "this command can destroy something". Used to ask for an
//! extra confirmation before `recall run` and to keep the built-in pack honest
//! (a test requires every pack command that trips this to be flagged).

/// Split a command into simple command segments on `;`, `&&`, `||`, `|` and
/// newlines, then into tokens. Deliberately naive: it errs on the side of
/// flagging, and quoting is not interpreted.
fn segments(cmd: &str) -> Vec<Vec<String>> {
    let normalized = cmd
        .replace("&&", "\n")
        .replace("||", "\n")
        .replace(['|', ';'], "\n");
    normalized
        .lines()
        .map(|l| l.split_whitespace().map(|t| t.to_string()).collect::<Vec<_>>())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Drop leading `sudo [opts]`, `doas`, `time`, `nohup`, `env`, `VAR=val`.
fn strip_prefix(tokens: &[String]) -> &[String] {
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if matches!(t, "sudo" | "doas" | "time" | "nohup" | "env" | "nice" | "ionice" | "command") {
            i += 1;
            while i < tokens.len() && tokens[i].starts_with('-') {
                let took_value = matches!(tokens[i].as_str(), "-u" | "-g");
                i += 1;
                if took_value {
                    i += 1; // sudo -u <user>
                }
            }
        } else if t.contains('=') && !t.starts_with('-') && !t.contains('/') && i + 1 < tokens.len() {
            i += 1; // VAR=value prefix
        } else {
            break;
        }
    }
    &tokens[i.min(tokens.len())..]
}

fn is_short_cluster(a: &str) -> bool {
    a.starts_with('-') && !a.starts_with("--") && a.len() > 1
}

/// Some short-flag cluster (`-rf`, `-Rvf`) contains any of `letters`.
fn short_has_any(args: &[String], letters: &str) -> bool {
    args.iter().any(|a| is_short_cluster(a) && letters.chars().any(|c| a[1..].contains(c)))
}

fn any_arg(args: &[String], f: impl Fn(&str) -> bool) -> bool {
    args.iter().any(|a| f(a))
}

fn is_arg(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Why this command is dangerous, if it looks so.
pub fn danger_reason(cmd: &str) -> Option<&'static str> {
    let compact: String = cmd.split_whitespace().collect::<Vec<_>>().join(" ");

    if compact.contains(":(){") || compact.contains(":() {") {
        return Some("fork bomb");
    }
    let fetches = compact.contains("curl ") || compact.contains("wget ");
    if fetches
        && ["| sh", "| bash", "| sudo sh", "| sudo bash", "|sh", "|bash"]
            .iter()
            .any(|p| compact.contains(p))
    {
        return Some("pipes a remote script straight into a shell");
    }
    if ["> /dev/sd", ">/dev/sd", "> /dev/nvme", "> /dev/disk"].iter().any(|p| compact.contains(p)) {
        return Some("overwrites a block device");
    }

    for seg in segments(cmd) {
        let toks = strip_prefix(&seg);
        let Some(first) = toks.first() else { continue };
        let args = &toks[1..];
        let prog = first.rsplit('/').next().unwrap_or(first);
        let sub = args.first().map(|s| s.as_str()).unwrap_or("");
        let rest = &args[args.len().min(1)..];

        match prog {
            "rm" => {
                let recursive = short_has_any(args, "rR") || is_arg(args, "--recursive");
                let force = short_has_any(args, "f") || is_arg(args, "--force");
                if is_arg(args, "--no-preserve-root") {
                    return Some("removes / without protection");
                }
                if recursive && force {
                    return Some("recursive forced delete");
                }
            }
            "dd" if any_arg(args, |a| a.starts_with("of=/dev/")) => return Some("writes raw data to a device"),
            "shred" | "wipefs" | "blkdiscard" | "srm" => return Some("irreversibly destroys data"),
            p if p.starts_with("mkfs") => return Some("creates a filesystem (erases the target)"),
            "mkswap" => return Some("erases the target"),
            "cryptsetup" if any_arg(args, |a| matches!(a, "luksFormat" | "luksErase" | "erase" | "luksKillSlot")) => {
                return Some("destroys LUKS key material / data")
            }
            "lvremove" | "vgremove" | "pvremove" => return Some("removes LVM volumes"),
            "zpool" if sub == "destroy" => return Some("destroys a ZFS pool"),
            "zfs" if sub == "destroy" => return Some("destroys ZFS data"),
            "btrfs" if args.windows(2).any(|w| w[0] == "subvolume" && w[1] == "delete") => {
                return Some("deletes a btrfs subvolume")
            }
            "mdadm" if any_arg(args, |a| a == "--zero-superblock" || a == "--stop") => {
                return Some("dismantles a RAID array")
            }
            "chmod" | "chown" | "chgrp"
                if (short_has_any(args, "R") || is_arg(args, "--recursive")) && any_arg(args, |a| a == "/" || a == "/*") =>
            {
                return Some("recursive permission change on /")
            }
            "chmod" if (short_has_any(args, "R") || is_arg(args, "--recursive")) && is_arg(args, "777") => {
                return Some("recursively opens permissions")
            }
            "crontab" if is_arg(args, "-r") => return Some("deletes all your cron jobs"),
            "userdel" if is_arg(args, "-r") || is_arg(args, "--remove") => return Some("removes an account and its home"),
            "reboot" | "poweroff" | "halt" | "shutdown" => return Some("shuts down or reboots the machine"),
            "init" if sub == "0" || sub == "6" => return Some("shuts down or reboots the machine"),
            "systemctl" if matches!(sub, "poweroff" | "reboot" | "halt") => return Some("shuts down or reboots the machine"),
            "iptables" | "ip6tables" if is_arg(args, "-F") || is_arg(args, "--flush") => {
                return Some("flushes firewall rules (can lock you out)")
            }
            "nft" if args.windows(2).any(|w| w[0] == "flush" && w[1] == "ruleset") => {
                return Some("flushes firewall rules (can lock you out)")
            }
            "ufw" if sub == "reset" => return Some("resets the firewall"),
            "rsync" if any_arg(args, |a| a.starts_with("--delete")) => return Some("deletes files at the destination"),
            "find" if is_arg(args, "-delete") => return Some("deletes every match"),
            "truncate" if is_arg(args, "-s0") || args.windows(2).any(|w| w[0] == "-s" && w[1] == "0") => {
                return Some("empties files")
            }
            "kill" | "pkill" | "killall" if is_arg(args, "-1") && (is_arg(args, "-9") || is_arg(args, "-KILL")) => {
                return Some("kills every process you may signal")
            }
            "git" => match sub {
                "push" if any_arg(rest, |a| a == "--force" || a == "-f" || a == "--force-with-lease" || a.starts_with('+')) => {
                    return Some("rewrites remote history")
                }
                "reset" if is_arg(rest, "--hard") => return Some("discards uncommitted work"),
                "clean" if short_has_any(rest, "f") => return Some("deletes untracked files"),
                "checkout" | "restore" if is_arg(rest, ".") => return Some("discards uncommitted changes"),
                "filter-branch" | "filter-repo" => return Some("rewrites the whole history"),
                "branch" if short_has_any(rest, "D") => return Some("force-deletes a branch"),
                "stash" if matches!(rest.first().map(|s| s.as_str()), Some("clear" | "drop")) => {
                    return Some("drops stashed work")
                }
                _ => {}
            },
            "docker" | "podman" => {
                let j = args.join(" ");
                if j.contains("system prune") || j.contains("volume prune") || j.contains("volume rm") {
                    return Some("deletes containers, images or volumes");
                }
                if j.contains("rm -f") || j.contains("rm --force") || j.contains("rmi -f") {
                    return Some("force-removes containers or images");
                }
            }
            "kubectl" if sub == "delete" && (is_arg(args, "--all") || is_arg(args, "--all-namespaces")) => {
                return Some("deletes every matching resource")
            }
            "terraform" | "tofu" if sub == "destroy" => return Some("destroys managed infrastructure"),
            "terraform" | "tofu" if is_arg(args, "-auto-approve") => return Some("applies without asking"),
            "aws" => {
                let j = args.join(" ");
                if (j.contains("s3 rm") && j.contains("--recursive"))
                    || j.contains("terminate-instances")
                    || j.contains("delete-")
                    || j.contains("s3 rb")
                {
                    return Some("deletes cloud resources");
                }
            }
            "az" | "gcloud" if is_arg(args, "delete") => return Some("deletes cloud resources"),
            "dropdb" => return Some("drops a database"),
            "mv" if is_arg(args, "/") || is_arg(args, "/*") => return Some("moves the root filesystem"),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_the_classics() {
        for c in [
            "rm -rf /tmp/x", "sudo rm -rf --no-preserve-root /", "rm -fr build", "rm -Rf build", "rm -r -f build",
            "dd if=/dev/zero of=/dev/sda bs=1M", "mkfs.ext4 /dev/sdb1", "shred -vfz /dev/sdc", "wipefs -a /dev/sdb",
            "echo x > /dev/sda", ":(){ :|:& };:", "curl -s https://x.sh | sudo bash", "git push --force origin main",
            "git reset --hard HEAD~3", "git clean -fdx", "docker system prune -af", "terraform destroy", "iptables -F",
            "crontab -r", "find / -name '*.tmp' -delete", "rsync -a --delete src/ dst/", "aws s3 rm s3://b --recursive",
            "cryptsetup luksFormat /dev/sdb1", "reboot", "userdel -r bob", "chmod -R 777 /", "kubectl delete pods --all",
            "sudo -u root rm -rf x", "git checkout .", "git branch -D old",
        ] {
            assert!(danger_reason(c).is_some(), "should flag: {}", c);
        }
    }

    #[test]
    fn leaves_ordinary_commands_alone() {
        for c in [
            "ls -la", "rm file.txt", "rm -r emptydir", "rm -f one.txt", "git status", "git push origin main",
            "git reset HEAD file", "docker ps -a", "dd if=/dev/sda of=disk.img bs=4M", "nmap -sV 10.0.0.1",
            "curl -s https://x.sh -o x.sh", "find . -name '*.log'", "rsync -av a/ b/", "chmod 644 f",
            "kubectl get pods", "terraform plan", "echo rm -rf", "grep -r foo .", "cat /etc/passwd | grep root",
            "git checkout main", "git branch -d merged",
        ] {
            assert!(danger_reason(c).is_none(), "should not flag: {}", c);
        }
    }

    #[test]
    fn looks_through_sudo_and_pipelines() {
        assert!(danger_reason("cd /tmp && sudo rm -rf build").is_some());
        assert!(danger_reason("ls | xargs echo; shred x").is_some());
        assert!(danger_reason("FOO=1 sudo -E rm -rf x").is_some());
    }
}
