//! Begrenzter, lesender Repository-Zugriff. Pfadschutz gilt vor jedem Öffnen,
//! auch bei Suchtreffern, Projektregeln und ungespeicherten Editorständen.

use ignore::{gitignore::Gitignore, WalkBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_FILE: usize = 512_000;
const MAX_OUTPUT: usize = 180_000;
const MAX_FILES: usize = 20_000;
const MAX_VERIFICATION_BYTES: u64 = 1_000_000_000;

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceSnapshot {
    pub fingerprint: String,
    pub files: BTreeMap<String, String>,
    pub bytes: u64,
    pub coverage: &'static str,
}

pub fn fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Overlay {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: String,
    pub path: String,
    pub sha256: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub editor: bool,
}

#[derive(Default)]
struct Evidence {
    sources: BTreeMap<String, Source>,
    instructions: BTreeMap<String, Value>,
    warnings: BTreeSet<String>,
    output_bytes: usize,
    delivered_instructions: BTreeSet<String>,
}

#[derive(Clone)]
pub struct Repository {
    root: PathBuf,
    excluded: Arc<Gitignore>,
    files: Arc<Vec<String>>,
    overlays: Arc<BTreeMap<String, String>>,
    evidence: Arc<Mutex<Evidence>>,
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Unabhängig von Ignore-Dateien gesperrt. Negationsregeln können diese
/// Grenze nicht aufheben; auch ein direkt angefragter Pfad bleibt gesperrt.
pub fn protected(path: &str) -> bool {
    path.replace('\\', "/").split('/').any(|part| {
        let p = part.to_lowercase();
        matches!(
            p.as_str(),
            ".git"
                | ".hg"
                | ".svn"
                | ".codegraph"
                | ".agentkit"
                | ".azure"
                | ".artifacts"
                | "node_modules"
                | "target"
                | "bin"
                | "obj"
                | "dist"
                | "out"
                | "logs"
                | "__pycache__"
        ) || p.starts_with(".env")
            || (p.starts_with("appsettings") && p.ends_with(".json"))
            || p.contains("credential")
            || p.contains("secret")
            || p.ends_with(".pem")
            || p.ends_with(".key")
            || p.ends_with(".pfx")
            || p.ends_with(".p12")
            || p.ends_with(".log")
            || p.ends_with(".pdf")
            || p.ends_with(".sqlite")
            || p.ends_with(".db")
    })
}

fn relative(path: &str) -> Result<PathBuf, String> {
    let normalized = path.replace('\\', "/");
    if normalized.contains(':')
        || normalized.starts_with('/')
        || normalized.split('/').any(|p| p == "..")
    {
        return Err(
            "Nur relative Pfade innerhalb des ausgewählten Repositorys sind erlaubt.".into(),
        );
    }
    let path = PathBuf::from(normalized);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err("Ungültiger Repository-Pfad.".into());
    }
    Ok(path)
}

impl Repository {
    /// Der Host bestimmt die Wurzel. Das Modell kann sie nicht als Tool-Argument
    /// ersetzen. Gitignore und .tandemignore gelten zusätzlich zu den Sperren.
    pub fn open(root: &Path, overlays: Vec<Overlay>) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|_| "Repository-Verzeichnis nicht verfügbar.")?;
        if !root.is_dir() {
            return Err("Die Repository-Wurzel ist kein Verzeichnis.".into());
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
        let custom = root.join(".tandemignore");
        if custom.exists() {
            let metadata = fs::symlink_metadata(&custom).map_err(|e| e.to_string())?;
            if is_link(&metadata) || metadata.len() > 64_000 {
                return Err(".tandemignore muss eine lokale Datei unter 64 KB sein.".into());
            }
            if let Some(error) = builder.add(&custom) {
                return Err(format!("Ungültige .tandemignore: {error}"));
            }
        }
        let mut repo = Self {
            root,
            excluded: Arc::new(builder.build().map_err(|e| e.to_string())?),
            files: Arc::new(vec![]),
            overlays: Arc::new(BTreeMap::new()),
            evidence: Arc::new(Mutex::new(Evidence::default())),
        };
        repo.files = Arc::new(repo.scan()?);
        let mut allowed = BTreeMap::new();
        for overlay in overlays {
            let p = repo.check_path(&overlay.path)?;
            if overlay.content.len() > MAX_FILE {
                return Err("Ein Editor-Snapshot überschreitet 512 KB.".into());
            }
            let key = repo.key(&p);
            allowed.insert(key, overlay.content);
        }
        repo.overlays = Arc::new(allowed);
        Ok(repo)
    }

    fn key(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn scan(&self) -> Result<Vec<String>, String> {
        let root = self.root.clone();
        let excluded = self.excluded.clone();
        let mut walk = WalkBuilder::new(&root);
        walk.hidden(false)
            .parents(false)
            .git_global(false)
            .require_git(false)
            .follow_links(false);
        walk.filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy();
            !protected(&rel)
                && !entry.file_type().is_some_and(|t| t.is_symlink())
                && !excluded
                    .matched_path_or_any_parents(
                        path,
                        entry.file_type().is_some_and(|t| t.is_dir()),
                    )
                    .is_ignore()
                && !fs::symlink_metadata(path)
                    .map(|m| is_link(&m))
                    .unwrap_or(true)
        });
        let mut files = vec![];
        for entry in walk.build() {
            let entry = entry.map_err(|e| format!("Repository nicht vollständig lesbar: {e}"))?;
            if entry.file_type().is_some_and(|t| t.is_file()) {
                files.push(self.key(entry.path()));
                if files.len() > MAX_FILES {
                    return Err("Mehr als 20.000 lesbare Dateien. Bitte einen kleineren Projektordner oder .tandemignore verwenden.".into());
                }
            }
        }
        files.sort();
        Ok(files)
    }

    pub fn check_path(&self, path: &str) -> Result<PathBuf, String> {
        let rel = relative(path)?;
        if protected(path) {
            return Err(
                "Dieser Pfad ist als Geheimnis, Laufzeitdaten oder generiertes Artefakt gesperrt."
                    .into(),
            );
        }
        let joined = self.root.join(&rel);
        let mut ancestor = self.root.clone();
        for component in rel.components() {
            ancestor.push(component);
            let metadata = fs::symlink_metadata(&ancestor).map_err(|_| "Pfad nicht verfügbar.")?;
            if is_link(&metadata) {
                return Err(
                    "Symlinks und Verzeichnisverknüpfungen sind für Repository-Zugriffe gesperrt."
                        .into(),
                );
            }
        }
        let real = joined.canonicalize().map_err(|_| "Pfad nicht verfügbar.")?;
        if !real.starts_with(&self.root)
            || self
                .excluded
                .matched_path_or_any_parents(&real, real.is_dir())
                .is_ignore()
        {
            return Err("Der Pfad ist außerhalb des Repositorys oder ausgeschlossen.".into());
        }
        if real.is_file() && !self.files.iter().any(|p| p == &self.key(&real)) {
            return Err("Der Pfad ist durch Repository-Ignore-Regeln ausgeschlossen.".into());
        }
        Ok(real)
    }

    fn text(&self, path: &str) -> Result<(String, String, bool), String> {
        let real = self.check_path(path)?;
        let metadata = fs::metadata(&real).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_FILE as u64 {
            return Err("Nur Textdateien bis 512 KB sind lesbar.".into());
        }
        if let Some(text) = self.overlays.get(&self.key(&real)) {
            return Ok((text.clone(), fingerprint(text.as_bytes()), true));
        }
        let bytes = fs::read(&real).map_err(|e| e.to_string())?;
        if bytes.contains(&0) {
            return Err("Binärdateien werden nicht an das Modell übergeben.".into());
        }
        let hash = fingerprint(&bytes);
        let text = String::from_utf8(bytes).map_err(|_| "Datei ist kein UTF-8-Text.")?;
        Ok((text, hash, false))
    }

    fn source(
        &self,
        path: &str,
        hash: &str,
        start: usize,
        end: usize,
        kind: &str,
        editor: bool,
    ) -> Source {
        let path = path.replace('\\', "/");
        let id = fingerprint(format!("{path}:{hash}").as_bytes());
        let mut evidence = self.evidence.lock().unwrap();
        let source = evidence
            .sources
            .entry(id.clone())
            .or_insert_with(|| Source {
                id,
                path,
                sha256: hash.into(),
                start_line: start,
                end_line: end,
                kind: kind.into(),
                editor,
            });
        source.start_line = source.start_line.min(start);
        source.end_line = source.end_line.max(end);
        source.clone()
    }

    fn charge(&self, value: Value) -> Result<Value, String> {
        let size = value.to_string().len();
        let mut evidence = self.evidence.lock().unwrap();
        if evidence.output_bytes + size > MAX_OUTPUT {
            return Err("Kontextbudget erreicht. Mit den vorhandenen Belegen abschließen oder die Aufgabe enger fassen.".into());
        }
        evidence.output_bytes += size;
        Ok(value)
    }

    pub fn list_files(&self, path: &str, filter: &str) -> Result<Value, String> {
        self.check_path(path)?;
        let prefix = path.replace('\\', "/").trim_matches('/').to_string();
        let needle = if filter == "*" {
            String::new()
        } else {
            filter.to_lowercase()
        };
        let files: Vec<_> = self
            .files
            .iter()
            .filter(|p| {
                (prefix.is_empty() || *p == &prefix || p.starts_with(&format!("{prefix}/")))
                    && p.to_lowercase().contains(&needle)
            })
            .collect();
        self.charge(json!({"files":files.iter().take(120).collect::<Vec<_>>(),"total":files.len(),"truncated":files.len()>120}))
    }

    pub fn read_file(&self, path: &str, start: usize, count: usize) -> Result<Value, String> {
        if start == 0 || count == 0 || count > 240 {
            return Err("start_line beginnt bei 1; line_count liegt zwischen 1 und 240.".into());
        }
        self.load_instructions(path)?;
        let (text, hash, editor) = self.text(path)?;
        let lines: Vec<_> = text.lines().collect();
        if start > lines.len().max(1) {
            return Err("Die Startzeile liegt hinter dem Dateiende.".into());
        }
        let end = (start - 1 + count).min(lines.len());
        let source = self.source(
            path,
            &hash,
            start,
            end.max(start),
            if path.to_lowercase().contains("test") {
                "test"
            } else {
                "code"
            },
            editor,
        );
        let excerpt = lines
            .iter()
            .enumerate()
            .skip(start - 1)
            .take(count)
            .map(|(i, line)| format!("{}\t{}", i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        self.charge(json!({"source":source,"content":excerpt,"total_lines":lines.len(),"instructions":self.new_instructions()}))
    }

    pub fn search(&self, query: &str, path: &str, references: bool) -> Result<Value, String> {
        if query.is_empty() || query.len() > 200 {
            return Err("Die Suchanfrage muss zwischen 1 und 200 Bytes lang sein.".into());
        }
        self.check_path(path)?;
        let prefix = path.replace('\\', "/").trim_matches('/').to_string();
        let needle = query.to_lowercase();
        let reference = if references {
            Some(
                regex::Regex::new(&format!(r"\b{}\b", regex::escape(query)))
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        let mut matches = vec![];
        let mut scanned = 0;
        let mut skipped = 0;
        let mut bytes = 0;
        let mut truncated = false;
        'files: for file in self
            .files
            .iter()
            .filter(|p| prefix.is_empty() || *p == &prefix || p.starts_with(&format!("{prefix}/")))
        {
            let Ok((text, hash, editor)) = self.text(file) else {
                skipped += 1;
                continue;
            };
            scanned += 1;
            bytes += text.len();
            if bytes > 40_000_000 {
                truncated = true;
                break;
            }
            for (index, line) in text.lines().enumerate() {
                let found = reference.as_ref().map_or_else(
                    || line.to_lowercase().contains(&needle),
                    |r| r.is_match(line),
                );
                if found {
                    self.load_instructions(file)?;
                    let source = self.source(
                        file,
                        &hash,
                        index + 1,
                        index + 1,
                        if file.to_lowercase().contains("test") {
                            "test"
                        } else {
                            "search"
                        },
                        editor,
                    );
                    matches.push(json!({"source":source,"line":index+1,"text":line.chars().take(400).collect::<String>()}));
                    if matches.len() == 60 {
                        truncated = true;
                        break 'files;
                    }
                }
            }
        }
        if skipped > 0 {
            self.evidence.lock().unwrap().warnings.insert(format!("{skipped} Dateien im Suchbereich waren keine lesbaren UTF-8-Dateien unter 512 KB. Kein vollständiger Suchnachweis."));
        }
        self.charge(json!({"matches":matches,"scanned_files":scanned,"skipped_files":skipped,"truncated":truncated,"semantics":if references {"Textuelle Symbolvorkommen; kein bewiesener Aufrufgraph"} else {"Textsuche"},"instructions":self.new_instructions()}))
    }

    pub fn load_instructions(&self, path: &str) -> Result<(), String> {
        let path = relative(path)?;
        let mut directories = vec![PathBuf::new()];
        let mut cursor = PathBuf::new();
        for component in path.parent().unwrap_or(Path::new("")).components() {
            cursor.push(component);
            directories.push(cursor.clone());
        }
        let mut pending = vec![];
        for directory in directories {
            for name in ["AGENTS.md", "CLAUDE.md"] {
                let file = directory.join(name);
                if self.root.join(&file).is_file() {
                    pending.push(file);
                }
            }
        }
        let links = regex::Regex::new(r"\]\(([^)#]+\.md)(?:#[^)]*)?\)").unwrap();
        while let Some(file) = pending.pop() {
            let key = file.to_string_lossy().replace('\\', "/");
            if self
                .evidence
                .lock()
                .unwrap()
                .instructions
                .contains_key(&key)
            {
                continue;
            }
            if self.evidence.lock().unwrap().instructions.len() >= 24 {
                return Err("Zu viele verknüpfte Projektregeln; Aufgabe enger fassen.".into());
            }
            let (text, hash, editor) = self.text(&key)?;
            if text.len() > 48_000 {
                return Err("Eine Projektregel überschreitet 48 KB.".into());
            }
            let source = self.source(
                &key,
                &hash,
                1,
                text.lines().count().max(1),
                "instruction",
                editor,
            );
            self.evidence
                .lock()
                .unwrap()
                .instructions
                .insert(key.clone(), json!({"source":source,"text":text}));
            for link in links.captures_iter(&text) {
                let linked = link[1].replace('\\', "/");
                let name = Path::new(&linked)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if !matches!(
                    name,
                    "AGENTS.md" | "CLAUDE.md" | "CLAUDE_BEHAVIORAL.md" | "CODING_GUIDELINES.md"
                ) {
                    continue;
                }
                // .. ist für explizite Regelverweise innerhalb der Wurzel erlaubt;
                // gewöhnliche Tools dürfen keine solchen Pfade übergeben.
                let joined = self
                    .root
                    .join(file.parent().unwrap_or(Path::new("")))
                    .join(&linked);
                let Ok(real) = joined.canonicalize() else {
                    continue;
                };
                if real.starts_with(&self.root) {
                    pending.push(real.strip_prefix(&self.root).unwrap().to_path_buf());
                }
            }
        }
        Ok(())
    }

    pub fn instructions(&self) -> Vec<Value> {
        self.evidence
            .lock()
            .unwrap()
            .instructions
            .values()
            .cloned()
            .collect()
    }
    pub fn new_instructions(&self) -> Vec<Value> {
        let mut evidence = self.evidence.lock().unwrap();
        let pending: Vec<_> = evidence
            .instructions
            .iter()
            .filter(|(key, _)| !evidence.delivered_instructions.contains(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        for (key, _) in &pending {
            evidence.delivered_instructions.insert(key.clone());
        }
        pending.into_iter().map(|(_, value)| value).collect()
    }
    pub fn sources(&self) -> Vec<Source> {
        self.evidence
            .lock()
            .unwrap()
            .sources
            .values()
            .cloned()
            .collect()
    }
    pub fn warnings(&self) -> Vec<String> {
        self.evidence
            .lock()
            .unwrap()
            .warnings
            .iter()
            .cloned()
            .collect()
    }

    /// Metadaten des Suchraums ergänzen die inhaltlich gebundenen Belege. Damit
    /// invalidiert auch eine neu angelegte relevante Datei den alten Suchstand.
    pub fn catalog_id(&self) -> Result<String, String> {
        let mut digest = Sha256::new();
        for file in self.scan()? {
            let metadata = fs::metadata(self.root.join(&file)).map_err(|e| e.to_string())?;
            digest.update(file.as_bytes());
            digest.update(metadata.len().to_le_bytes());
            digest.update(
                metadata
                    .modified()
                    .map_err(|e| e.to_string())?
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .to_le_bytes(),
            );
        }
        for (path, content) in self.overlays.iter() {
            digest.update(path.as_bytes());
            digest.update(content.as_bytes());
        }
        Ok(format!("{:x}", digest.finalize()))
    }

    /// Inhaltsbindung des gesamten erlaubten Suchraums für reale Prüfungen.
    /// Liefert nur Hashes, niemals zusätzliche Quelltexte an ein Modell. Eine
    /// unvollständige oder während des Lesens veränderte Basis ist kein Beleg.
    pub fn workspace_snapshot(&self) -> Result<WorkspaceSnapshot, String> {
        let catalog = self.catalog_id()?;
        let mut files = BTreeMap::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; 65_536];
        for path in self.files.iter() {
            let real = self.check_path(path)?;
            let mut file = fs::File::open(&real).map_err(|e| e.to_string())?;
            let mut digest = Sha256::new();
            loop {
                let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
                if count == 0 {
                    break;
                }
                bytes += count as u64;
                if bytes > MAX_VERIFICATION_BYTES {
                    return Err("Die Inhaltsprüfung überschreitet 1 GB. Bitte den Projektordner oder den erlaubten Suchraum mit .tandemignore konkret eingrenzen; kein unvollständiger Prüfbeleg.".into());
                }
                digest.update(&buffer[..count]);
            }
            files.insert(path.clone(), format!("{:x}", digest.finalize()));
        }
        if self.catalog_id()? != catalog {
            return Err("Der Projektstand hat sich während der Inhaltsprüfung geändert. Bitte erneut prüfen.".into());
        }
        let fingerprint = fingerprint(&serde_json::to_vec(&files).map_err(|e| e.to_string())?);
        Ok(WorkspaceSnapshot {fingerprint,files,bytes,coverage:"Alle Dateien des erlaubten Repository-Suchraums; Gitignore, .tandemignore, feste Pfadsperren und Link-Sperren gelten. Kein Betriebssystem-Sandboxnachweis."})
    }

    pub fn validate(&self, sources: &[Source], catalog_id: &str) -> Result<bool, String> {
        if self.catalog_id()? != catalog_id {
            return Ok(false);
        }
        for source in sources {
            let Ok((_, hash, editor)) = self.text(&source.path) else {
                return Ok(false);
            };
            if hash != source.sha256 || editor != source.editor {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
