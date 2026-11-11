//! Security scan by an AI agent: Spatium starts Claude Code or Codex with a read-only
//! brief and shows what it reports. The app does no checking of its own, and neither
//! it nor the agent deletes or changes anything; the user decides what to remove.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

#[derive(Clone, Copy, PartialEq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    fn bin(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// Absolute path of the CLI. An app opened from Finder has a short PATH, so the
    /// usual install folders are searched as well.
    pub fn find(self) -> Option<PathBuf> {
        let h = home();
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        dirs.extend([h.join(".local/bin"), h.join(".claude/local"), h.join(".bun/bin"), h.join(".npm-global/bin"), "/opt/homebrew/bin".into(), "/usr/local/bin".into()]);
        if let Ok(rd) = std::fs::read_dir(h.join(".nvm/versions/node")) {
            let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("bin")).collect();
            v.sort();
            dirs.extend(v.into_iter().rev());
        }
        dirs.into_iter().map(|d| d.join(self.bin())).find(|p| p.is_file())
    }
}

/// What the agent is asked to look at.
#[derive(Clone, PartialEq)]
pub enum Scope {
    /// Start-up items, applications, downloads, shell start-up files, running processes.
    Mac,
    Folder(PathBuf),
}

const RULES: &str = "Work strictly read-only: do NOT delete, move, rename, modify, quarantine, unload, kill or install anything, \
do not run anything you find, and do not use the network. \
Report in this order: (1) a two-sentence overall verdict; (2) each item that deserves attention with its full path, \
what it most likely is (name the vendor or app if you recognise it), your verdict (malicious / suspicious / cracked or pirated / unwanted / benign) \
with the evidence you saw, and what the user could do by hand; (3) the number of items you judged benign, without listing them. \
Be honest about uncertainty: this is a review by reading files and signatures, not an antivirus scan.";

pub fn prompt(scope: &Scope) -> String {
    let h = home().display().to_string();
    match scope {
        Scope::Mac => format!(
            "Check this macOS computer for malware, trojans, adware, cracked software and unwanted persistence. {RULES}\n\n\
Look at least at: launch agents and daemons ({h}/Library/LaunchAgents, /Library/LaunchAgents, /Library/LaunchDaemons) and the programs they start; \
/Library/PrivilegedHelperTools; code signatures (codesign -dv) of those programs and of the apps in /Applications and {h}/Applications \
(unsigned, ad-hoc, a signature that no longer verifies, or a certificate not issued by Apple such as \"TNT\" are red flags); \
programs, scripts, installers and disk images in {h}/Downloads, {h}/Desktop, /Users/Shared and /private/tmp; \
shell start-up files ({h}/.zshrc, .zprofile, .bash_profile, .profile) for lines that download or decode code and run it \
(report only the file and line number, never the line itself: these files hold API keys); \
running processes (ps) with odd names or locations; browser extensions if you can find them. \
Programs in hidden or temporary folders deserve a close look."
        ),
        Scope::Folder(p) => format!(
            "Check the folder {} (and its subfolders) for malware, trojans, adware, cracked software and suspicious scripts. {RULES}\n\n\
Look at executables, app bundles, scripts, installers, disk images and documents with macros: their code signatures (codesign -dv), \
quarantine flags (xattr), file types (file), and script contents. A large folder may be sampled: say what you covered.",
            p.display()
        ),
    }
}

/// A running agent scan; its text arrives line by line.
pub struct Review {
    pub agent: Agent,
    pub scope: Scope,
    pub text: Arc<Mutex<String>>,
    pub done: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Review {
    pub fn stop(&self) {
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

/// Where the last report is saved.
pub fn report_path() -> PathBuf {
    std::env::temp_dir().join("spatium-security/report.txt")
}

pub fn start(agent: Agent, scope: Scope) -> Result<Review, String> {
    let exe = agent.find().ok_or_else(|| format!("{} CLI ({}) not found", agent.name(), agent.bin()))?;
    let p = prompt(&scope);
    let work = std::env::temp_dir().join("spatium-security");
    let _ = std::fs::create_dir_all(&work);
    let _ = std::fs::write(work.join("prompt.txt"), &p);
    let mut cmd = Command::new(&exe);
    match agent {
        Agent::Claude => {
            cmd.args(["-p", &p, "--output-format", "stream-json", "--verbose", "--allowedTools"]);
            // read-only tools only; anything else is refused in -p mode
            cmd.args([
                "Read",
                "Glob",
                "Grep",
                "Bash(codesign:*)",
                "Bash(spctl:*)",
                "Bash(shasum:*)",
                "Bash(xattr -p:*)",
                "Bash(xattr -l:*)",
                "Bash(plutil -p:*)",
                "Bash(file:*)",
                "Bash(ls:*)",
                "Bash(stat:*)",
                "Bash(ps:*)",
                "Bash(mdls:*)",
                "Bash(mdfind:*)",
                "Bash(pkgutil --check-signature:*)",
                "Bash(launchctl list:*)",
                "Bash(systemextensionsctl list:*)",
            ]);
            cmd.args(["--disallowedTools", "Write", "Edit", "NotebookEdit", "WebFetch", "WebSearch"]);
            // use the Claude subscription login, not a stale API key from the shell
            cmd.env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        Agent::Codex => {
            cmd.args(["exec", "--skip-git-repo-check", "--sandbox", "read-only", &p]);
        }
    }
    // node-based CLIs need their own folder on PATH when launched from Finder
    if let Some(dir) = exe.parent() {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{path}:/usr/bin:/bin:/usr/sbin:/sbin", dir.display()));
    }
    cmd.current_dir(&work).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let text = Arc::new(Mutex::new(String::new()));
    let done = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(Some(child)));
    {
        let (text, done, child) = (text.clone(), done.clone(), child.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let add = match agent {
                    Agent::Claude => claude_line(&line),
                    Agent::Codex => Some(format!("{line}\n")),
                };
                if let Some(a) = add {
                    text.lock().unwrap().push_str(&a);
                }
            }
            let mut e = String::new();
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                e.push_str(&l);
                e.push('\n');
            }
            let status = child.lock().unwrap().as_mut().and_then(|c| c.wait().ok());
            if status.map(|s| !s.success()).unwrap_or(false) {
                let mut t = text.lock().unwrap();
                t.push_str("\n[the agent stopped with an error]\n");
                t.push_str(&e.chars().take(2000).collect::<String>());
            }
            let _ = std::fs::write(report_path(), &*text.lock().unwrap());
            done.store(true, Ordering::Relaxed);
        });
    }
    Ok(Review { agent, scope, text, done, child })
}

/// One line of `claude -p --output-format stream-json`: the assistant's text, and a
/// short note for each command it runs.
fn claude_line(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    match v.get("type")?.as_str()? {
        "assistant" => {
            let mut s = String::new();
            for c in v.pointer("/message/content")?.as_array()? {
                match c.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        s.push_str(c.get("text")?.as_str()?);
                        s.push('\n');
                    }
                    Some("tool_use") => {
                        let input = c.get("input");
                        let what = input
                            .and_then(|i| i.get("command").or_else(|| i.get("file_path")).or_else(|| i.get("pattern")))
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let what: String = what.chars().take(140).collect();
                        s.push_str(&format!("  › {} {what}\n", c.get("name").and_then(|n| n.as_str()).unwrap_or("tool")));
                    }
                    _ => {}
                }
            }
            Some(s)
        }
        "result" if v.get("is_error").and_then(|x| x.as_bool()) == Some(true) => {
            Some(format!("\n[error] {}\n", v.get("result").and_then(|r| r.as_str()).unwrap_or("")))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_stream_lines() {
        let l = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Verdict"},{"type":"tool_use","name":"Bash","input":{"command":"codesign -dv x"}}]}}"#;
        let s = claude_line(l).unwrap();
        assert!(s.contains("Verdict") && s.contains("› Bash codesign -dv x"));
        assert!(claude_line(r#"{"type":"system"}"#).is_none());
    }

    #[test]
    fn prompts_are_read_only() {
        for s in [Scope::Mac, Scope::Folder("/tmp/x".into())] {
            assert!(prompt(&s).contains("do NOT delete"));
        }
    }
}
