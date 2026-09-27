//! `--patch`: der Agent arbeitet auf einer Kopie des Workspaces, und heraus
//! kommt ein Unified Diff statt geänderter Dateien.
//!
//! Zwei Teile, beide ohne neue Abhängigkeit: [`Worktree`] legt die Kopie an
//! und vergleicht sie am Ende mit dem Original, [`unified_diff`] schreibt die
//! Unterschiede einer Datei im Format, das `git apply` und `patch -p1` lesen.
//!
//! Was hier NICHT isoliert ist: `run_shell` läuft in der Kopie, kann aber wie
//! immer auch außerhalb des Workspaces wirken (`rm ~/x`). Die Kopie schützt
//! den Workspace, sie ist keine Sandbox.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Ordner, die ohne Git-Index weder kopiert noch verglichen werden: Rauschen
/// (Build-Ausgaben, Abhängigkeiten), das niemand im Patch sehen will.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
];

/// Kontextzeilen je Hunk — wie `diff -u` und `git diff`.
const CONTEXT: usize = 3;

/// Eine Arbeitskopie des Workspaces in einem Temp-Verzeichnis. Wird beim
/// Drop wieder gelöscht.
pub struct Worktree {
    original: PathBuf,
    copy: PathBuf,
    root: PathBuf,
    /// Liegt der Workspace in einem Git-Repo? Dann bestimmt Git, welche
    /// Dateien dazugehören (`.gitignore` gilt), sonst [`SKIP_DIRS`].
    git: bool,
    files: BTreeSet<String>,
}

impl Worktree {
    /// Kopiert den Workspace. Im Git-Repo genau die Dateien, die Git kennt
    /// oder nicht ignoriert (`ls-files -co --exclude-standard`) — so bleibt ein
    /// `target/` mit Gigabytes draußen. Symlinks werden nicht kopiert.
    pub fn create(workspace: &Path) -> io::Result<Worktree> {
        let original = workspace.canonicalize()?;
        let (git, files) = match git_files(&original) {
            Some(files) => (true, files),
            None => (false, walk(&original, &original)),
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("agentkit-patch-{}-{stamp}", std::process::id()));
        let name = original
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "ws".into());
        let copy = root.join(name);
        std::fs::create_dir_all(&copy)?;
        let mut kopiert = BTreeSet::new();
        for rel in &files {
            let src = original.join(rel);
            let Ok(meta) = std::fs::symlink_metadata(&src) else {
                continue; // gelistet, aber gelöscht (Git-Index kennt noch)
            };
            if !meta.is_file() {
                continue;
            }
            let dst = copy.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dst)?;
            kopiert.insert(rel.clone());
        }
        Ok(Worktree {
            original,
            copy,
            root,
            git,
            files: kopiert,
        })
    }

    /// Pfad der Kopie — dorthin zeigt der Workspace des Agenten.
    pub fn path(&self) -> &Path {
        &self.copy
    }

    /// Der Patch von Original zu Kopie, plus Hinweise zu Dateien, die sich
    /// nicht als Text darstellen ließen (binär oder kein UTF-8).
    pub fn diff(&self) -> io::Result<(String, Vec<String>)> {
        let mut paths: BTreeSet<String> = self.files.clone();
        let mut neu: Vec<String> = walk(&self.copy, &self.copy)
            .into_iter()
            .filter(|p| !self.files.contains(p))
            .collect();
        if self.git {
            // Build-Ausgaben, die der Agent beim Testen erzeugt hat, gehören
            // nicht in den Patch: Git entscheidet nach den Regeln des Originals.
            let ignoriert = git_ignored(&self.original, &neu);
            neu.retain(|p| !ignoriert.contains(p));
        }
        paths.extend(neu);

        let mut patch = String::new();
        let mut hinweise = Vec::new();
        for rel in &paths {
            let alt = read_opt(&self.original.join(rel))?;
            let neu = read_opt(&self.copy.join(rel))?;
            if alt == neu {
                continue;
            }
            let text = |b: &Option<Vec<u8>>| -> Result<Option<String>, ()> {
                match b {
                    None => Ok(None),
                    Some(bytes) if bytes.contains(&0) => Err(()),
                    Some(bytes) => String::from_utf8(bytes.clone()).map(Some).map_err(|_| ()),
                }
            };
            match (text(&alt), text(&neu)) {
                (Ok(a), Ok(b)) => patch.push_str(&unified_diff(rel, a.as_deref(), b.as_deref())),
                _ => hinweise.push(format!("{rel}: Binärdatei geändert — nicht im Patch")),
            }
        }
        Ok((patch, hinweise))
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn read_opt(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => std::fs::read(path).map(Some),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Dateien, die Git im Workspace kennt oder nicht ignoriert. `None`, wenn der
/// Workspace in keinem Repo liegt (oder `git` fehlt).
fn git_files(ws: &Path) -> Option<BTreeSet<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).to_string())
            .collect(),
    )
}

/// Welche der (relativen) Pfade ignoriert Git im Original? Funktioniert auch
/// für Pfade, die dort gar nicht existieren — genau der Fall hier.
fn git_ignored(ws: &Path, paths: &[String]) -> BTreeSet<String> {
    if paths.is_empty() {
        return BTreeSet::new();
    }
    let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["check-ignore", "--stdin", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return BTreeSet::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let mut input = Vec::new();
        for p in paths {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        let _ = stdin.write_all(&input);
    }
    let Ok(out) = child.wait_with_output() else {
        return BTreeSet::new();
    };
    out.stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).to_string())
        .collect()
}

/// Alle Dateien unter `dir` als `/`-getrennte Pfade relativ zu `base`,
/// ohne [`SKIP_DIRS`] und ohne Symlinks.
fn walk(base: &Path, dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let name = entry.file_name();
                if !SKIP_DIRS.iter().any(|s| name == *s) {
                    stack.push(p);
                }
            } else if kind.is_file() {
                if let Ok(rel) = p.strip_prefix(base) {
                    let rel: Vec<String> = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().to_string())
                        .collect();
                    out.insert(rel.join("/"));
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ Diff

#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Eq,
    Del,
    Ins,
}

/// Unified Diff EINER Datei im Git-Format (`a/`/`b/`-Präfixe, `/dev/null` für
/// neue und gelöschte Dateien). `None` heißt „existiert nicht". Leer, wenn
/// beide Seiten gleich sind.
pub fn unified_diff(path: &str, old: Option<&str>, new: Option<&str>) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<&str> = old
        .map(|t| t.split_inclusive('\n').collect())
        .unwrap_or_default();
    let b: Vec<&str> = new
        .map(|t| t.split_inclusive('\n').collect())
        .unwrap_or_default();
    let mut out = format!("diff --git a/{path} b/{path}\n");
    match (old, new) {
        (None, _) => out.push_str(&format!(
            "new file mode 100644\n--- /dev/null\n+++ b/{path}\n"
        )),
        (_, None) => out.push_str(&format!(
            "deleted file mode 100644\n--- a/{path}\n+++ /dev/null\n"
        )),
        _ => out.push_str(&format!("--- a/{path}\n+++ b/{path}\n")),
    }
    let ops = diff_ops(&a, &b);
    // Position (alt, neu) VOR jeder Operation — daraus kommen die Hunk-Köpfe.
    let mut pos = Vec::with_capacity(ops.len() + 1);
    let (mut i, mut j) = (0usize, 0usize);
    for op in &ops {
        pos.push((i, j));
        match op {
            Op::Eq => {
                i += 1;
                j += 1;
            }
            Op::Del => i += 1,
            Op::Ins => j += 1,
        }
    }
    pos.push((i, j));

    let changes: Vec<usize> = (0..ops.len()).filter(|k| ops[*k] != Op::Eq).collect();
    let mut c = 0;
    while c < changes.len() {
        let start = changes[c].saturating_sub(CONTEXT);
        let mut last = changes[c];
        // Änderungen, zwischen denen höchstens 2×CONTEXT gleiche Zeilen
        // liegen, teilen sich einen Hunk (wie bei diff -u).
        while c + 1 < changes.len() && changes[c + 1] <= last + 2 * CONTEXT + 1 {
            c += 1;
            last = changes[c];
        }
        c += 1;
        let end = (last + CONTEXT + 1).min(ops.len());
        let (os, ns) = pos[start];
        let (oe, ne) = pos[end];
        let (ol, nl) = (oe - os, ne - ns);
        let head = |s: usize, l: usize| if l == 0 { s } else { s + 1 };
        out.push_str(&format!(
            "@@ -{},{ol} +{},{nl} @@\n",
            head(os, ol),
            head(ns, nl)
        ));
        for k in start..end {
            let (ai, bj) = pos[k];
            let (prefix, line) = match ops[k] {
                Op::Eq => (' ', a[ai]),
                Op::Del => ('-', a[ai]),
                Op::Ins => ('+', b[bj]),
            };
            out.push(prefix);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// Kürzeste Edit-Folge (Myers, O((N+M)·D)). Gemeinsamer Anfang und Ende
/// werden vorab abgeschnitten; wird die Suche zu teuer (riesige, völlig
/// verschiedene Dateien), gilt die ganze Mitte als ersetzt — ein korrekter,
/// nur nicht minimaler Patch.
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut ops = vec![Op::Eq; pre];
    ops.extend(myers(am, bm));
    ops.extend(std::iter::repeat(Op::Eq).take(suf));
    ops
}

fn myers(a: &[&str], b: &[&str]) -> Vec<Op> {
    /// Obergrenze für den gemerkten Suchverlauf (Einträge). Darüber lohnt die
    /// minimale Lösung den Speicher nicht.
    const TRACE_LIMIT: usize = 20_000_000;
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let off = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=max as isize {
        if trace.len() * v.len() > TRACE_LIMIT {
            break;
        }
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let Some(dmax) = found else {
        let mut ops = vec![Op::Del; a.len()];
        ops.extend(std::iter::repeat(Op::Ins).take(b.len()));
        return ops;
    };
    let (mut x, mut y) = (n, m);
    let mut rev = Vec::new();
    for d in (0..=dmax).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && v[(k - 1 + off) as usize] < v[(k + 1 + off) as usize])
        {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v[(prev_k + off) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            rev.push(Op::Eq);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            rev.push(if x == prev_x { Op::Ins } else { Op::Del });
        }
        x = prev_x;
        y = prev_y;
    }
    rev.reverse();
    rev
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gleiche_inhalte_ergeben_keinen_diff() {
        assert_eq!(unified_diff("a", Some("x\n"), Some("x\n")), "");
    }

    #[test]
    fn aenderung_mit_kontext() {
        let alt = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let neu = "1\n2\n3\n4\nfünf\n6\n7\n8\n9\n";
        let d = unified_diff("src/z.txt", Some(alt), Some(neu));
        assert_eq!(
            d,
            "diff --git a/src/z.txt b/src/z.txt\n--- a/src/z.txt\n+++ b/src/z.txt\n\
             @@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+fünf\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn neue_und_geloeschte_datei() {
        let d = unified_diff("n.txt", None, Some("a\nb\n"));
        assert!(
            d.contains(
                "new file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1,2 @@\n+a\n+b\n"
            ),
            "{d}"
        );
        let d = unified_diff("g.txt", Some("a\n"), None);
        assert!(
            d.contains(
                "deleted file mode 100644\n--- a/g.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-a\n"
            ),
            "{d}"
        );
    }

    #[test]
    fn fehlender_zeilenumbruch_wird_markiert() {
        let d = unified_diff("f", Some("a\nb"), Some("a\nb\n"));
        assert!(d.ends_with("-b\n\\ No newline at end of file\n+b\n"), "{d}");
    }

    #[test]
    fn weit_auseinander_liegende_aenderungen_ergeben_zwei_hunks() {
        let alt: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let neu: String = (1..=30)
            .map(|i| match i {
                3 => "drei\n".to_string(),
                27 => "x\n".to_string(),
                _ => format!("{i}\n"),
            })
            .collect();
        let d = unified_diff("f", Some(&alt), Some(&neu));
        assert_eq!(d.matches("@@ -").count(), 2, "{d}");
        // Die Zeilen, die als Kontext gelten, müssen stimmen.
        assert!(d.contains("@@ -1,6 +1,6 @@"), "{d}");
    }

    #[test]
    fn myers_liefert_minimale_folge() {
        let a = ["a\n", "b\n", "c\n", "a\n", "b\n", "b\n", "a\n"];
        let b = ["c\n", "b\n", "a\n", "b\n", "a\n", "c\n"];
        let ops = diff_ops(&a, &b);
        let edits = ops.iter().filter(|o| **o != Op::Eq).count();
        assert_eq!(edits, 5, "klassisches Myers-Beispiel: D = 5");
    }

    /// Kopie anlegen, darin ändern, Patch prüfen — außerhalb eines Git-Repos
    /// (Temp-Verzeichnis), also über den Verzeichnis-Walk.
    #[test]
    fn worktree_liefert_patch_und_laesst_original_unberuehrt() {
        let ws = std::env::temp_dir().join(format!("agentkit_patch_ws_{}", std::process::id()));
        std::fs::remove_dir_all(&ws).ok();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(
            ws.join("src/main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        std::fs::write(ws.join("alt.txt"), "weg\n").unwrap();
        std::fs::write(ws.join("target/gross.bin"), "build").unwrap();

        let wt = Worktree::create(&ws).unwrap();
        assert!(
            !wt.path().join("target").exists(),
            "Build-Ordner wird nicht kopiert"
        );
        std::fs::write(
            wt.path().join("src/main.rs"),
            "fn main() {\n    log::info!(\"hi\");\n}\n",
        )
        .unwrap();
        std::fs::remove_file(wt.path().join("alt.txt")).unwrap();
        std::fs::write(wt.path().join("neu.txt"), "da\n").unwrap();

        let (patch, hinweise) = wt.diff().unwrap();
        assert!(hinweise.is_empty());
        assert!(
            patch.contains("-    println!(\"hi\");\n+    log::info!(\"hi\");\n"),
            "{patch}"
        );
        assert!(patch.contains("+++ /dev/null"), "{patch}");
        assert!(patch.contains("+++ b/neu.txt"), "{patch}");
        // Das Original bleibt, wie es war.
        assert!(std::fs::read_to_string(ws.join("src/main.rs"))
            .unwrap()
            .contains("println!"));
        assert!(ws.join("alt.txt").exists());

        let kopie = wt.path().to_path_buf();
        drop(wt);
        assert!(!kopie.exists(), "die Kopie wird beim Drop gelöscht");
        std::fs::remove_dir_all(&ws).ok();
    }
}
