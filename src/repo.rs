//! Repository analysis — tree-sitter parsing, symbol extraction, and git churn scoring.
//!
//! This module provides a [`RepoMap`] that summarises every symbol and churn
//! hotspot in a codebase. The map is designed to be compact enough to fit
//! inside Mercury 2's context window while still giving the model a useful
//! structural overview of the code it is about to edit.

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command;

use git2::Repository;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use tree_sitter::Parser;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors originating from repository analysis.
#[derive(Error, Debug)]
pub enum RepoError {
    /// An I/O error (reading files, walking directories).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Tree-sitter failed to produce a parse tree for the given file.
    #[error("tree-sitter parse failed for {0}")]
    ParseFailed(String),

    /// A libgit2 error.
    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    /// A git command failed while preparing an isolated workspace.
    #[error("git command failed (`{command}`): {stderr}")]
    GitCommandFailed { command: String, stderr: String },

    /// A planner or runtime path was not a safe repo-relative path.
    #[error("invalid repo-relative path `{path}`: {reason}")]
    InvalidRepoRelativePath { path: String, reason: String },
}

/// Convenience alias used throughout this module.
pub type Result<T> = std::result::Result<T, RepoError>;

/// A validated path that is always relative to a repository root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoRelativePath(String);

impl RepoRelativePath {
    /// Validate a repo-relative path without a specific filesystem root.
    pub fn new(path: impl AsRef<str>) -> Result<Self> {
        let normalized = normalize_repo_relative_path(path.as_ref())?;
        Ok(Self(normalized))
    }

    /// Validate a repo-relative path against an existing repository root.
    pub fn from_planner_path(project_root: &Path, path: impl AsRef<str>) -> Result<Self> {
        let relative = Self::new(path)?;
        relative.ensure_within_root(project_root)?;
        Ok(relative)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn resolve_under(&self, root: &Path) -> Result<PathBuf> {
        resolve_repo_relative_path(root, self.as_path(), self.as_str())
    }

    pub fn ensure_within_root(&self, root: &Path) -> Result<()> {
        self.resolve_under(root).map(|_| ())
    }
}

impl std::fmt::Display for RepoRelativePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for RepoRelativePath {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RepoRelativePath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

fn normalize_repo_relative_path(raw: &str) -> Result<String> {
    if raw.trim().is_empty() {
        return Err(invalid_repo_relative_path(raw, "path cannot be empty"));
    }
    if raw.contains('\0') {
        return Err(invalid_repo_relative_path(raw, "path contains NUL byte"));
    }
    if looks_like_windows_absolute_path(raw) {
        return Err(invalid_repo_relative_path(
            raw,
            "absolute Windows paths are not allowed",
        ));
    }

    let normalized_separators = raw.replace('\\', "/");
    let candidate = Path::new(&normalized_separators);
    if candidate.is_absolute() {
        return Err(invalid_repo_relative_path(
            raw,
            "absolute paths are not allowed",
        ));
    }

    let mut components = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => components.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid_repo_relative_path(
                    raw,
                    "parent-directory traversal is not allowed",
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid_repo_relative_path(
                    raw,
                    "path must stay relative to the repository root",
                ));
            }
        }
    }

    if components.is_empty() {
        return Err(invalid_repo_relative_path(
            raw,
            "path must point to a repository file",
        ));
    }

    Ok(components.join("/"))
}

fn looks_like_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || path.starts_with("\\\\")
}

fn resolve_repo_relative_path(root: &Path, relative: &Path, raw: &str) -> Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let mut current = root.clone();
    let mut components = relative.components().peekable();

    while let Some(component) = components.next() {
        let Component::Normal(part) = component else {
            return Err(invalid_repo_relative_path(
                raw,
                "path contains unsupported components",
            ));
        };
        let next = current.join(part);
        match fs::symlink_metadata(&next) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    let target = fs::canonicalize(&next)?;
                    if !target.starts_with(&root) {
                        return Err(invalid_repo_relative_path(
                            raw,
                            "symlink escapes the repository root",
                        ));
                    }
                    current = target;
                } else {
                    if components.peek().is_some() && !metadata.is_dir() {
                        return Err(invalid_repo_relative_path(
                            raw,
                            "path traverses through a file",
                        ));
                    }
                    current = next;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                current = next;
            }
            Err(err) => return Err(err.into()),
        }
    }

    if !current.starts_with(&root) {
        return Err(invalid_repo_relative_path(
            raw,
            "path resolves outside the repository root",
        ));
    }

    Ok(current)
}

fn invalid_repo_relative_path(path: &str, reason: &str) -> RepoError {
    RepoError::InvalidRepoRelativePath {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Symbol types
// ---------------------------------------------------------------------------

/// The category of a code symbol extracted by tree-sitter.
#[derive(Debug, Clone, PartialEq)]
pub enum SymbolKind {
    /// A `fn` item.
    Function,
    /// A `struct` item.
    Struct,
    /// An `impl` block.
    Impl,
    /// A `trait` definition.
    Trait,
    /// An `enum` definition.
    Enum,
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SymbolKind::Function => write!(f, "fn"),
            SymbolKind::Struct => write!(f, "struct"),
            SymbolKind::Impl => write!(f, "impl"),
            SymbolKind::Trait => write!(f, "trait"),
            SymbolKind::Enum => write!(f, "enum"),
        }
    }
}

/// A single code symbol extracted from a supported source file.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// The identifier (e.g. function name, struct name).
    pub name: String,
    /// What kind of symbol this is.
    pub kind: SymbolKind,
    /// Path to the file that contains the symbol.
    pub file_path: String,
    /// 1-based start line.
    pub line_start: u32,
    /// 1-based end line (inclusive).
    pub line_end: u32,
}

// ---------------------------------------------------------------------------
// Churn types
// ---------------------------------------------------------------------------

/// Git churn score for a single file, derived from recent commit history.
#[derive(Debug, Clone)]
pub struct ChurnScore {
    /// Path of the file relative to the repo root.
    pub file_path: String,
    /// Number of commits that touched this file.
    pub commit_count: u32,
    /// Total number of lines added + deleted across those commits.
    pub lines_changed: u32,
    /// Score normalised to the range `[0.0, 1.0]` relative to the
    /// highest-churn file in the repository.
    pub normalized_score: f64,
}

// ---------------------------------------------------------------------------
// RepoMap
// ---------------------------------------------------------------------------

/// A compact summary of a codebase's structure and change frequency,
/// suitable for injection into Mercury 2's context window.
#[derive(Debug, Clone)]
pub struct RepoMap {
    /// All symbols extracted from indexed source files.
    ///
    /// Rust symbols come from tree-sitter parsing; TypeScript symbols are
    /// collected with token-aware source scanning.
    pub symbols: Vec<Symbol>,
    /// Per-file git churn scores.
    pub churn_scores: Vec<ChurnScore>,
    /// Total number of indexed source files across enabled languages.
    pub indexed_file_count: usize,
    /// Per-language indexed file counts.
    pub language_file_counts: HashMap<String, usize>,
    /// Total line count across all indexed source files.
    pub total_lines: usize,
}

/// Supported languages for repository walking/indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Go,
    Java,
}

impl Language {
    fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "ts" | "tsx" | "mts" | "cts" => Some(Self::TypeScript),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Go => "go",
            Self::Java => "java",
        }
    }
}

fn parser_for_language(language: Language) -> Option<Parser> {
    match language {
        Language::Rust => {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .ok()?;
            Some(parser)
        }
        Language::Python | Language::TypeScript | Language::Go | Language::Java => None,
    }
}

/// Language controls for repository indexing.
#[derive(Debug, Clone)]
pub struct RepoLanguages {
    pub rust: bool,
    pub python: bool,
    pub typescript: bool,
    pub go: bool,
    pub java: bool,
}

impl Default for RepoLanguages {
    fn default() -> Self {
        Self {
            rust: true,
            python: false,
            typescript: false,
            go: false,
            java: false,
        }
    }
}

impl RepoLanguages {
    fn is_enabled(&self, lang: Language) -> bool {
        match lang {
            Language::Rust => self.rust,
            Language::Python => self.python,
            Language::TypeScript => self.typescript,
            Language::Go => self.go,
            Language::Java => self.java,
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace isolation
// ---------------------------------------------------------------------------

/// Prepare an isolated repair workspace rooted at `workspace_root`.
///
/// When `project_root` is a git repository, this creates a detached git
/// worktree to avoid repository copies and guarantee candidate isolation from
/// the user's primary working directory.
///
/// If the project is not a git repo, this falls back to a filtered copy so
/// local tests and non-git fixtures still work.
pub fn prepare_repair_workspace(
    project_root: &Path,
    workspace_root: &Path,
    accepted_states: &HashMap<RepoRelativePath, String>,
) -> Result<()> {
    if workspace_root.exists() {
        let _ = cleanup_repair_workspace(project_root, workspace_root);
    }
    if let Some(parent) = workspace_root.parent() {
        fs::create_dir_all(parent)?;
    }

    if Repository::discover(project_root).is_ok() {
        create_detached_worktree(project_root, workspace_root)?;
    } else {
        fs::create_dir_all(workspace_root)?;
        copy_workspace_tree(project_root, workspace_root, project_root)?;
    }

    for (relative_path, content) in accepted_states {
        let destination = relative_path.resolve_under(workspace_root)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(destination, content)?;
    }

    Ok(())
}

/// Remove an isolated repair workspace if it exists.
///
/// For git-backed workspaces this first unregisters the worktree from git,
/// then removes any remaining on-disk directory.
pub fn cleanup_repair_workspace(project_root: &Path, workspace_root: &Path) -> Result<()> {
    if !workspace_root.exists() {
        return Ok(());
    }

    if Repository::discover(project_root).is_ok() {
        let output = Command::new("git")
            .arg("-C")
            .arg(project_root)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(workspace_root)
            .output()?;
        if !output.status.success() && workspace_root.exists() {
            fs::remove_dir_all(workspace_root)?;
        }

        // Keep `.git/worktrees` metadata tidy for short-lived candidate roots.
        let _ = Command::new("git")
            .arg("-C")
            .arg(project_root)
            .arg("worktree")
            .arg("prune")
            .arg("--expire")
            .arg("now")
            .status();
    } else {
        fs::remove_dir_all(workspace_root)?;
    }

    Ok(())
}

fn create_detached_worktree(project_root: &Path, workspace_root: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .arg("worktree")
        .arg("add")
        .arg("--detach")
        .arg("--force")
        .arg(workspace_root)
        .arg("HEAD")
        .output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let command = format!(
        "git -C {} worktree add --detach --force {} HEAD",
        project_root.display(),
        workspace_root.display()
    );
    Err(RepoError::GitCommandFailed { command, stderr })
}

fn copy_workspace_tree(source: &Path, destination: &Path, root: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let relative = source_path
            .strip_prefix(root)
            .unwrap_or(source_path.as_path());

        if should_skip_workspace_copy(relative) {
            continue;
        }

        let destination_path = destination.join(relative);
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            fs::create_dir_all(&destination_path)?;
            copy_workspace_tree(&source_path, destination, root)?;
        } else if metadata.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source_path, &destination_path)?;
        }
    }

    Ok(())
}

fn should_skip_workspace_copy(relative: &Path) -> bool {
    let relative = relative.to_string_lossy();
    relative == ".git"
        || relative.starts_with(".git/")
        || relative == "target"
        || relative.starts_with("target/")
        || relative == ".mercury/worktrees"
        || relative.starts_with(".mercury/worktrees/")
        || relative == ".mercury/runs"
        || relative.starts_with(".mercury/runs/")
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a single Rust source string and return extracted symbols.
///
/// `path` is recorded verbatim in each [`Symbol::file_path`] field; it does
/// not need to point to a real file on disk.
pub fn parse_file(path: &str, source: &str) -> Result<Vec<Symbol>> {
    let mut parser = parser_for_language(Language::Rust)
        .ok_or_else(|| RepoError::ParseFailed(path.to_string()))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| RepoError::ParseFailed(path.to_string()))?;

    let mut symbols = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();

    // Walk only top-level children of the root node.
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            let kind_str = node.kind();

            let symbol_kind = match kind_str {
                "function_item" => Some(SymbolKind::Function),
                "struct_item" => Some(SymbolKind::Struct),
                "impl_item" => Some(SymbolKind::Impl),
                "trait_item" => Some(SymbolKind::Trait),
                "enum_item" => Some(SymbolKind::Enum),
                _ => None,
            };

            if let Some(kind) = symbol_kind {
                // For impl blocks the "name" field may be absent; fall back
                // to the "type" field which holds the Self type.
                let name = node
                    .child_by_field_name("name")
                    .or_else(|| node.child_by_field_name("type"))
                    .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                    .unwrap_or("<anonymous>")
                    .to_string();

                symbols.push(Symbol {
                    name,
                    kind,
                    file_path: path.to_string(),
                    // tree-sitter rows are 0-based; we store 1-based lines.
                    line_start: node.start_position().row as u32 + 1,
                    line_end: node.end_position().row as u32 + 1,
                });
            }

            // Also walk into `impl` blocks to find nested function items.
            if kind_str == "impl_item" {
                let mut inner = node.walk();
                if inner.goto_first_child() {
                    loop {
                        let child = inner.node();
                        if child.kind() == "declaration_list" {
                            let mut dl_cursor = child.walk();
                            if dl_cursor.goto_first_child() {
                                loop {
                                    let item = dl_cursor.node();
                                    if item.kind() == "function_item" {
                                        let fn_name = item
                                            .child_by_field_name("name")
                                            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                                            .unwrap_or("<anonymous>")
                                            .to_string();

                                        symbols.push(Symbol {
                                            name: fn_name,
                                            kind: SymbolKind::Function,
                                            file_path: path.to_string(),
                                            line_start: item.start_position().row as u32 + 1,
                                            line_end: item.end_position().row as u32 + 1,
                                        });
                                    }
                                    if !dl_cursor.goto_next_sibling() {
                                        break;
                                    }
                                }
                            }
                        }
                        if !inner.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    Ok(symbols)
}

/// Recursively walk `dir_path`, parse enabled language files, and return all symbols.
pub fn parse_directory(dir_path: &str) -> Result<Vec<Symbol>> {
    parse_directory_with_languages(dir_path, &RepoLanguages::default())
}

/// Recursively walk `dir_path`, parse source files for enabled languages,
/// and return extracted symbols.
pub fn parse_directory_with_languages(
    dir_path: &str,
    languages: &RepoLanguages,
) -> Result<Vec<Symbol>> {
    let mut all_symbols = Vec::new();
    walk_source_files(Path::new(dir_path), languages, &mut |path, language| {
        let source = fs::read_to_string(path)?;
        let path_str = path.to_string_lossy().to_string();
        let syms = parse_file_for_language(&path_str, &source, language)?;
        all_symbols.extend(syms);
        Ok(())
    })?;
    sort_symbols(&mut all_symbols);
    Ok(all_symbols)
}

fn parse_file_for_language(path: &str, source: &str, language: Language) -> Result<Vec<Symbol>> {
    match language {
        Language::Rust => parse_file(path, source),
        Language::TypeScript => Ok(parse_typescript_file(path, source)),
        Language::Python | Language::Go | Language::Java => Ok(Vec::new()),
    }
}

fn parse_typescript_file(path: &str, source: &str) -> Vec<Symbol> {
    let masked = mask_typescript_non_code(source);
    let tokens = tokenize_typescript(&masked);
    let mut symbols = collect_typescript_symbols(path, &tokens);
    sort_symbols(&mut symbols);
    symbols
}

fn new_symbol_with_lines(
    path: &str,
    name: String,
    kind: SymbolKind,
    line_start: u32,
    line_end: u32,
) -> Symbol {
    Symbol {
        name,
        kind,
        file_path: path.to_string(),
        line_start,
        line_end: line_end.max(line_start),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TsTokenKind {
    Identifier(String),
    Punct(char),
    Arrow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TsToken {
    kind: TsTokenKind,
    line: u32,
}

const TYPESCRIPT_DECLARATION_MODIFIERS: &[&str] =
    &["export", "default", "declare", "async", "abstract"];

fn mask_typescript_non_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = String::with_capacity(source.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"//") {
            masked.push(' ');
            masked.push(' ');
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                masked.push(' ');
                i += 1;
            }
            continue;
        }

        if bytes[i..].starts_with(b"/*") {
            masked.push(' ');
            masked.push(' ');
            i += 2;
            while i < bytes.len() {
                if bytes[i..].starts_with(b"*/") {
                    masked.push(' ');
                    masked.push(' ');
                    i += 2;
                    break;
                }
                if bytes[i] == b'\n' {
                    masked.push('\n');
                } else {
                    masked.push(' ');
                }
                i += 1;
            }
            continue;
        }

        if matches!(bytes[i], b'\'' | b'"' | b'`') {
            let quote = bytes[i];
            masked.push(' ');
            i += 1;
            let mut escaped = false;
            while i < bytes.len() {
                let byte = bytes[i];
                if byte == b'\n' {
                    masked.push('\n');
                    escaped = false;
                    i += 1;
                    continue;
                }

                masked.push(' ');
                i += 1;

                if !escaped && byte == quote {
                    break;
                }

                escaped = !escaped && byte == b'\\';
            }
            continue;
        }

        masked.push(bytes[i] as char);
        i += 1;
    }

    masked
}

fn tokenize_typescript(source: &str) -> Vec<TsToken> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line = 1_u32;

    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b if b.is_ascii_whitespace() => i += 1,
            b'=' if bytes.get(i + 1) == Some(&b'>') => {
                tokens.push(TsToken {
                    kind: TsTokenKind::Arrow,
                    line,
                });
                i += 2;
            }
            b if is_typescript_identifier_start(b) => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_typescript_identifier_continue(bytes[i]) {
                    i += 1;
                }
                tokens.push(TsToken {
                    kind: TsTokenKind::Identifier(source[start..i].to_string()),
                    line,
                });
            }
            b if matches!(
                b as char,
                '{' | '}'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | ';'
                    | ','
                    | ':'
                    | '='
                    | '<'
                    | '>'
                    | '?'
                    | '!'
                    | '*'
                    | '.'
            ) =>
            {
                tokens.push(TsToken {
                    kind: TsTokenKind::Punct(b as char),
                    line,
                });
                i += 1;
            }
            _ => i += 1,
        }
    }

    tokens
}

fn is_typescript_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphabetic()
}

fn is_typescript_identifier_continue(byte: u8) -> bool {
    is_typescript_identifier_start(byte) || byte.is_ascii_digit()
}

fn collect_typescript_symbols(path: &str, tokens: &[TsToken]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        if token_is_punct(tokens, i, '{') {
            if let Some(end) = find_matching_punct(tokens, i, '{', '}') {
                i = end + 1;
                continue;
            }
            break;
        }

        let decl_idx = skip_typescript_modifiers(tokens, i, TYPESCRIPT_DECLARATION_MODIFIERS);
        let next = match identifier_at(tokens, decl_idx) {
            Some("function") => parse_function_declaration(path, tokens, decl_idx, "default")
                .map(|symbol| {
                    symbols.push(symbol);
                    advance_past_body_or_semicolon(tokens, decl_idx)
                })
                .unwrap_or(decl_idx + 1),
            Some("class") => {
                let (class_symbol, next_idx) =
                    parse_declaration_with_body(path, tokens, decl_idx, SymbolKind::Struct);
                if let Some(symbol) = class_symbol {
                    symbols.push(symbol);
                }
                next_idx
            }
            Some("interface") => {
                let (trait_symbol, next_idx) =
                    parse_declaration_with_body(path, tokens, decl_idx, SymbolKind::Trait);
                if let Some(symbol) = trait_symbol {
                    symbols.push(symbol);
                }
                next_idx
            }
            Some("enum") => {
                let (enum_symbol, next_idx) =
                    parse_declaration_with_body(path, tokens, decl_idx, SymbolKind::Enum);
                if let Some(symbol) = enum_symbol {
                    symbols.push(symbol);
                }
                next_idx
            }
            Some("const" | "let" | "var") => {
                let (statement_symbols, next_idx) =
                    parse_variable_statement(path, tokens, decl_idx);
                symbols.extend(statement_symbols);
                next_idx
            }
            _ => i + 1,
        };

        i = next.max(i + 1);
    }

    symbols
}

fn parse_function_declaration(
    path: &str,
    tokens: &[TsToken],
    function_idx: usize,
    anonymous_name: &str,
) -> Option<Symbol> {
    let line_start = tokens.get(function_idx)?.line;
    let mut cursor = function_idx + 1;
    if token_is_punct(tokens, cursor, '*') {
        cursor += 1;
    }

    let name = identifier_at(tokens, cursor)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| anonymous_name.to_string());
    let line_end = find_body_or_semicolon_line_end(tokens, function_idx).unwrap_or(line_start);
    Some(new_symbol_with_lines(
        path,
        name,
        SymbolKind::Function,
        line_start,
        line_end,
    ))
}

fn parse_declaration_with_body(
    path: &str,
    tokens: &[TsToken],
    decl_idx: usize,
    kind: SymbolKind,
) -> (Option<Symbol>, usize) {
    let line_start = tokens.get(decl_idx).map(|token| token.line).unwrap_or(1);
    let name = identifier_at(tokens, decl_idx + 1)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "default".to_string());
    let line_end = find_body_or_semicolon_line_end(tokens, decl_idx).unwrap_or(line_start);
    let next_idx = advance_past_body_or_semicolon(tokens, decl_idx);
    (
        Some(new_symbol_with_lines(
            path, name, kind, line_start, line_end,
        )),
        next_idx,
    )
}

fn parse_variable_statement(
    path: &str,
    tokens: &[TsToken],
    start_idx: usize,
) -> (Vec<Symbol>, usize) {
    let stmt_end = find_statement_end(tokens, start_idx).unwrap_or(tokens.len().saturating_sub(1));
    let mut cursor = start_idx + 1;
    let mut symbols = Vec::new();

    while cursor <= stmt_end && cursor < tokens.len() {
        cursor = skip_until_identifier_or_delimiter(tokens, cursor, stmt_end);
        if cursor > stmt_end || token_is_punct(tokens, cursor, ';') {
            break;
        }

        let Some(name) = identifier_at(tokens, cursor).map(ToOwned::to_owned) else {
            cursor += 1;
            continue;
        };
        let line_start = tokens[cursor].line;
        cursor += 1;

        while token_is_punct(tokens, cursor, '?') || token_is_punct(tokens, cursor, '!') {
            cursor += 1;
        }

        if token_is_punct(tokens, cursor, ':') {
            cursor = skip_type_annotation(tokens, cursor + 1, stmt_end);
        }

        if !token_is_punct(tokens, cursor, '=') {
            cursor = advance_to_next_declarator(tokens, cursor, stmt_end);
            continue;
        }

        let init_start = cursor + 1;
        let decl_end = find_declarator_end(tokens, init_start, stmt_end);
        if let Some((kind, line_end)) = classify_variable_initializer(tokens, init_start, decl_end)
        {
            symbols.push(new_symbol_with_lines(
                path, name, kind, line_start, line_end,
            ));
        }
        cursor = decl_end.saturating_add(1);
    }

    (symbols, stmt_end.saturating_add(1))
}

fn classify_variable_initializer(
    tokens: &[TsToken],
    init_start: usize,
    decl_end: usize,
) -> Option<(SymbolKind, u32)> {
    if init_start > decl_end || init_start >= tokens.len() {
        return None;
    }

    let cursor = skip_typescript_modifiers(tokens, init_start, &["async"]);
    if matches!(identifier_at(tokens, cursor), Some("function")) {
        return Some((
            SymbolKind::Function,
            find_body_or_semicolon_line_end(tokens, cursor).unwrap_or(tokens[cursor].line),
        ));
    }

    if matches!(identifier_at(tokens, cursor), Some("class")) {
        return Some((
            SymbolKind::Struct,
            find_body_or_semicolon_line_end(tokens, cursor).unwrap_or(tokens[cursor].line),
        ));
    }

    let mut paren_depth = 0_u32;
    let mut bracket_depth = 0_u32;
    let mut brace_depth = 0_u32;
    let mut angle_depth = 0_u32;
    for idx in init_start..=decl_end.min(tokens.len().saturating_sub(1)) {
        match &tokens[idx].kind {
            TsTokenKind::Punct('(') => paren_depth += 1,
            TsTokenKind::Punct(')') => paren_depth = paren_depth.saturating_sub(1),
            TsTokenKind::Punct('[') => bracket_depth += 1,
            TsTokenKind::Punct(']') => bracket_depth = bracket_depth.saturating_sub(1),
            TsTokenKind::Punct('{') => brace_depth += 1,
            TsTokenKind::Punct('}') => brace_depth = brace_depth.saturating_sub(1),
            TsTokenKind::Punct('<') => angle_depth += 1,
            TsTokenKind::Punct('>') => angle_depth = angle_depth.saturating_sub(1),
            TsTokenKind::Arrow
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                let line_end = find_arrow_initializer_line_end(tokens, idx + 1, decl_end)
                    .unwrap_or(tokens[idx].line);
                return Some((SymbolKind::Function, line_end));
            }
            _ => {}
        }
    }

    None
}

fn find_arrow_initializer_line_end(
    tokens: &[TsToken],
    start_idx: usize,
    decl_end: usize,
) -> Option<u32> {
    if start_idx > decl_end || start_idx >= tokens.len() {
        return None;
    }

    if token_is_punct(tokens, start_idx, '{') {
        return find_matching_punct(tokens, start_idx, '{', '}').map(|idx| tokens[idx].line);
    }

    Some(tokens[decl_end.min(tokens.len().saturating_sub(1))].line)
}

fn skip_until_identifier_or_delimiter(tokens: &[TsToken], mut idx: usize, end_idx: usize) -> usize {
    while idx <= end_idx && idx < tokens.len() {
        if matches!(tokens[idx].kind, TsTokenKind::Identifier(_))
            || token_is_punct(tokens, idx, ';')
            || token_is_punct(tokens, idx, ',')
        {
            break;
        }

        if token_is_punct(tokens, idx, '{') || token_is_punct(tokens, idx, '[') {
            idx = skip_balanced_block(tokens, idx, end_idx).unwrap_or(idx + 1);
            continue;
        }

        idx += 1;
    }
    idx
}

fn skip_type_annotation(tokens: &[TsToken], mut idx: usize, end_idx: usize) -> usize {
    let mut paren_depth = 0_u32;
    let mut bracket_depth = 0_u32;
    let mut brace_depth = 0_u32;
    let mut angle_depth = 0_u32;

    while idx <= end_idx && idx < tokens.len() {
        match &tokens[idx].kind {
            TsTokenKind::Punct('(') => paren_depth += 1,
            TsTokenKind::Punct(')') => paren_depth = paren_depth.saturating_sub(1),
            TsTokenKind::Punct('[') => bracket_depth += 1,
            TsTokenKind::Punct(']') => bracket_depth = bracket_depth.saturating_sub(1),
            TsTokenKind::Punct('{') => brace_depth += 1,
            TsTokenKind::Punct('}') => brace_depth = brace_depth.saturating_sub(1),
            TsTokenKind::Punct('<') => angle_depth += 1,
            TsTokenKind::Punct('>') => angle_depth = angle_depth.saturating_sub(1),
            TsTokenKind::Punct('=' | ',' | ';')
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                break;
            }
            _ => {}
        }
        idx += 1;
    }

    idx
}

fn find_statement_end(tokens: &[TsToken], start_idx: usize) -> Option<usize> {
    let mut paren_depth = 0_u32;
    let mut bracket_depth = 0_u32;
    let mut brace_depth = 0_u32;
    let mut angle_depth = 0_u32;
    let mut idx = start_idx;

    while idx < tokens.len() {
        match tokens[idx].kind {
            TsTokenKind::Punct('(') => paren_depth += 1,
            TsTokenKind::Punct(')') => paren_depth = paren_depth.saturating_sub(1),
            TsTokenKind::Punct('[') => bracket_depth += 1,
            TsTokenKind::Punct(']') => bracket_depth = bracket_depth.saturating_sub(1),
            TsTokenKind::Punct('{') => brace_depth += 1,
            TsTokenKind::Punct('}') => brace_depth = brace_depth.saturating_sub(1),
            TsTokenKind::Punct('<') => angle_depth += 1,
            TsTokenKind::Punct('>') => angle_depth = angle_depth.saturating_sub(1),
            TsTokenKind::Punct(';')
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                return Some(idx);
            }
            _ => {}
        }
        idx += 1;
    }

    None
}

fn find_declarator_end(tokens: &[TsToken], start_idx: usize, stmt_end: usize) -> usize {
    find_terminator_with_limit(tokens, start_idx, stmt_end, true).unwrap_or(stmt_end)
}

fn advance_to_next_declarator(tokens: &[TsToken], mut idx: usize, stmt_end: usize) -> usize {
    while idx <= stmt_end && idx < tokens.len() {
        if token_is_punct(tokens, idx, ',') || token_is_punct(tokens, idx, ';') {
            return idx + 1;
        }
        idx += 1;
    }
    stmt_end.saturating_add(1)
}

fn find_body_or_semicolon_line_end(tokens: &[TsToken], start_idx: usize) -> Option<u32> {
    let mut paren_depth = 0_u32;
    let mut bracket_depth = 0_u32;
    let mut angle_depth = 0_u32;
    let mut idx = start_idx;

    while idx < tokens.len() {
        match tokens[idx].kind {
            TsTokenKind::Punct('(') => paren_depth += 1,
            TsTokenKind::Punct(')') => paren_depth = paren_depth.saturating_sub(1),
            TsTokenKind::Punct('[') => bracket_depth += 1,
            TsTokenKind::Punct(']') => bracket_depth = bracket_depth.saturating_sub(1),
            TsTokenKind::Punct('<') => angle_depth += 1,
            TsTokenKind::Punct('>') => angle_depth = angle_depth.saturating_sub(1),
            TsTokenKind::Punct('{')
                if paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 =>
            {
                return find_matching_punct(tokens, idx, '{', '}').map(|end| tokens[end].line);
            }
            TsTokenKind::Punct(';')
                if paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 =>
            {
                return Some(tokens[idx].line);
            }
            _ => {}
        }
        idx += 1;
    }

    tokens.last().map(|token| token.line)
}

fn advance_past_body_or_semicolon(tokens: &[TsToken], start_idx: usize) -> usize {
    let mut paren_depth = 0_u32;
    let mut bracket_depth = 0_u32;
    let mut angle_depth = 0_u32;
    let mut idx = start_idx;

    while idx < tokens.len() {
        match tokens[idx].kind {
            TsTokenKind::Punct('(') => paren_depth += 1,
            TsTokenKind::Punct(')') => paren_depth = paren_depth.saturating_sub(1),
            TsTokenKind::Punct('[') => bracket_depth += 1,
            TsTokenKind::Punct(']') => bracket_depth = bracket_depth.saturating_sub(1),
            TsTokenKind::Punct('<') => angle_depth += 1,
            TsTokenKind::Punct('>') => angle_depth = angle_depth.saturating_sub(1),
            TsTokenKind::Punct('{')
                if paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 =>
            {
                return find_matching_punct(tokens, idx, '{', '}')
                    .map(|end| end + 1)
                    .unwrap_or(tokens.len());
            }
            TsTokenKind::Punct(';')
                if paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 =>
            {
                return idx + 1;
            }
            _ => {}
        }
        idx += 1;
    }

    tokens.len()
}

fn find_terminator_with_limit(
    tokens: &[TsToken],
    start_idx: usize,
    end_idx: usize,
    stop_on_semicolon: bool,
) -> Option<usize> {
    let mut paren_depth = 0_u32;
    let mut bracket_depth = 0_u32;
    let mut brace_depth = 0_u32;
    let mut angle_depth = 0_u32;
    let mut idx = start_idx;

    while idx <= end_idx && idx < tokens.len() {
        match tokens[idx].kind {
            TsTokenKind::Punct('(') => paren_depth += 1,
            TsTokenKind::Punct(')') => paren_depth = paren_depth.saturating_sub(1),
            TsTokenKind::Punct('[') => bracket_depth += 1,
            TsTokenKind::Punct(']') => bracket_depth = bracket_depth.saturating_sub(1),
            TsTokenKind::Punct('{') => brace_depth += 1,
            TsTokenKind::Punct('}') => brace_depth = brace_depth.saturating_sub(1),
            TsTokenKind::Punct('<') => angle_depth += 1,
            TsTokenKind::Punct('>') => angle_depth = angle_depth.saturating_sub(1),
            TsTokenKind::Punct(',')
                if paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                return Some(idx);
            }
            TsTokenKind::Punct(';')
                if stop_on_semicolon
                    && paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0 =>
            {
                return Some(idx);
            }
            _ => {}
        }
        idx += 1;
    }

    None
}

fn skip_balanced_block(tokens: &[TsToken], start_idx: usize, end_idx: usize) -> Option<usize> {
    match &tokens.get(start_idx)?.kind {
        TsTokenKind::Punct('{') => {
            find_matching_punct(tokens, start_idx, '{', '}').map(|idx| idx + 1)
        }
        TsTokenKind::Punct('[') => {
            find_matching_punct(tokens, start_idx, '[', ']').map(|idx| idx + 1)
        }
        _ => {
            let _ = end_idx;
            None
        }
    }
}

fn find_matching_punct(
    tokens: &[TsToken],
    start_idx: usize,
    open: char,
    close: char,
) -> Option<usize> {
    if !token_is_punct(tokens, start_idx, open) {
        return None;
    }

    let mut depth = 0_u32;
    for (idx, token) in tokens.iter().enumerate().skip(start_idx) {
        match token.kind {
            TsTokenKind::Punct(ch) if ch == open => depth += 1,
            TsTokenKind::Punct(ch) if ch == close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

fn skip_typescript_modifiers(tokens: &[TsToken], mut idx: usize, modifiers: &[&str]) -> usize {
    while matches!(identifier_at(tokens, idx), Some(name) if modifiers.contains(&name)) {
        idx += 1;
    }
    idx
}

fn identifier_at(tokens: &[TsToken], idx: usize) -> Option<&str> {
    match &tokens.get(idx)?.kind {
        TsTokenKind::Identifier(name) => Some(name.as_str()),
        _ => None,
    }
}

fn token_is_punct(tokens: &[TsToken], idx: usize, punct: char) -> bool {
    matches!(tokens.get(idx).map(|token| &token.kind), Some(TsTokenKind::Punct(ch)) if *ch == punct)
}

fn sort_symbols(symbols: &mut Vec<Symbol>) {
    symbols.sort_by(cmp_symbols);
    symbols.dedup_by(|left, right| {
        left.file_path == right.file_path
            && left.line_start == right.line_start
            && left.line_end == right.line_end
            && left.kind == right.kind
            && left.name == right.name
    });
}

fn cmp_symbols(left: &Symbol, right: &Symbol) -> std::cmp::Ordering {
    left.file_path
        .cmp(&right.file_path)
        .then(left.line_start.cmp(&right.line_start))
        .then(left.line_end.cmp(&right.line_end))
        .then(symbol_kind_order(&left.kind).cmp(&symbol_kind_order(&right.kind)))
        .then(left.name.cmp(&right.name))
}

fn symbol_kind_order(kind: &SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Struct => 1,
        SymbolKind::Impl => 2,
        SymbolKind::Trait => 3,
        SymbolKind::Enum => 4,
    }
}

fn should_skip_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(".git") | Some("target") | Some(".mercury")
    )
}

/// Internal helper that recursively visits source files under `dir` keyed by
/// extension and language enablement.
fn walk_source_files(
    dir: &Path,
    languages: &RepoLanguages,
    visitor: &mut dyn FnMut(&Path, Language) -> Result<()>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if should_skip_directory(&path) {
                continue;
            }
            walk_source_files(&path, languages, visitor)?;
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(language) = Language::from_extension(ext) {
                if languages.is_enabled(language) {
                    visitor(&path, language)?;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Git churn
// ---------------------------------------------------------------------------

/// Walk the last 100 commits of the repository at `repo_path` and compute
/// per-file churn scores.
///
/// The `normalized_score` field of each [`ChurnScore`] is scaled so that
/// the file with the highest raw churn receives `1.0` and all others are
/// proportionally smaller.
pub fn git_churn_scores(repo_path: &str) -> Result<Vec<ChurnScore>> {
    let repo = Repository::open(repo_path)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push_head()?;
    revwalk.set_sorting(git2::Sort::TIME)?;

    // Accumulate per-file stats: (commit_count, lines_changed).
    let mut stats: HashMap<String, (u32, u32)> = HashMap::new();

    const MAX_COMMITS: u32 = 100;

    for (commits_seen, oid_result) in (0_u32..).zip(revwalk) {
        if commits_seen >= MAX_COMMITS {
            break;
        }
        let oid = oid_result?;
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;

        // Diff against the first parent (or an empty tree for the root commit).
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

        let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;

        // Collect per-file delta stats from the diff.
        let diff_stats = diff.stats()?;
        let _ = diff_stats; // we iterate deltas instead for per-file info

        let num_deltas = diff.deltas().len();
        for delta_idx in 0..num_deltas {
            let delta = diff.get_delta(delta_idx);
            if let Some(delta) = delta {
                let file_path = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(|p| p.to_string_lossy().to_string());

                if let Some(fp) = file_path {
                    let entry = stats.entry(fp).or_insert((0, 0));
                    entry.0 += 1; // commit_count

                    // Walk hunks to count changed lines for this delta.
                    let mut lines_in_delta: u32 = 0;
                    let patch = git2::Patch::from_diff(&diff, delta_idx);
                    if let Ok(Some(patch)) = patch {
                        let (_, additions, deletions) = patch.line_stats().unwrap_or((0, 0, 0));
                        lines_in_delta = (additions + deletions) as u32;
                    }
                    entry.1 += lines_in_delta;
                }
            }
        }
    }

    // Find max raw score for normalisation.
    let max_raw: u32 = stats.values().map(|(c, l)| c + l).max().unwrap_or(1);
    let max_f64 = max_raw as f64;

    let mut scores: Vec<ChurnScore> = stats
        .into_iter()
        .map(|(file_path, (commit_count, lines_changed))| {
            let raw = (commit_count + lines_changed) as f64;
            let normalized_score = if max_f64 > 0.0 { raw / max_f64 } else { 0.0 };
            ChurnScore {
                file_path,
                commit_count,
                lines_changed,
                normalized_score,
            }
        })
        .collect();

    // Sort descending by normalised score for deterministic output.
    scores.sort_by(|a, b| {
        b.normalized_score
            .partial_cmp(&a.normalized_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(scores)
}

// ---------------------------------------------------------------------------
// RepoMap construction
// ---------------------------------------------------------------------------

/// Build a complete [`RepoMap`] for the repository at `dir_path`.
///
/// This combines [`parse_directory`] (symbol extraction) with
/// [`git_churn_scores`] (change-frequency analysis). If `dir_path` is not
/// inside a git repository the churn scores will be empty rather than
/// returning an error.
pub fn build_repo_map(dir_path: &str) -> Result<RepoMap> {
    build_repo_map_with_languages(dir_path, &RepoLanguages::default())
}

pub fn build_repo_map_with_languages(dir_path: &str, languages: &RepoLanguages) -> Result<RepoMap> {
    let symbols = parse_directory_with_languages(dir_path, languages)?;

    // Count files and total lines.
    let mut indexed_file_count: usize = 0;
    let mut language_file_counts: HashMap<String, usize> = HashMap::new();
    let mut total_lines: usize = 0;
    walk_source_files(Path::new(dir_path), languages, &mut |path, language| {
        indexed_file_count += 1;
        *language_file_counts
            .entry(language.as_str().to_string())
            .or_insert(0) += 1;
        let content = fs::read_to_string(path)?;
        total_lines += content.lines().count();
        Ok(())
    })?;

    // Git churn is best-effort; a missing `.git` folder should not be fatal.
    let churn_scores = git_churn_scores(dir_path).unwrap_or_default();

    Ok(RepoMap {
        symbols,
        churn_scores,
        indexed_file_count,
        language_file_counts,
        total_lines,
    })
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format a [`RepoMap`] as a concise plain-text summary suitable for
/// feeding into Mercury 2's context window.
///
/// The output groups symbols by file, lists each symbol's kind and line
/// range, and appends a churn-score section at the end.
pub fn format_repo_map(map: &RepoMap) -> String {
    let mut out = String::new();

    // Header
    let _ = writeln!(
        out,
        "=== Repository Map ({} files, {} lines) ===\n",
        map.indexed_file_count, map.total_lines,
    );

    if !map.language_file_counts.is_empty() {
        let mut language_keys: Vec<&String> = map.language_file_counts.keys().collect();
        language_keys.sort();
        let stats = language_keys
            .into_iter()
            .map(|k| format!("{}={}", k, map.language_file_counts[k]))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "Languages indexed: {stats}\n");
    }

    // Group symbols by file.
    let mut by_file: HashMap<&str, Vec<&Symbol>> = HashMap::new();
    for sym in &map.symbols {
        by_file.entry(&sym.file_path).or_default().push(sym);
    }
    for syms in by_file.values_mut() {
        syms.sort_by(|left, right| cmp_symbols(left, right));
    }

    // Sort file keys for deterministic output.
    let mut file_keys: Vec<&&str> = by_file.keys().collect();
    file_keys.sort();

    for file in file_keys {
        let _ = writeln!(out, "--- {} ---", file);
        let syms = by_file.get(*file).expect("key came from map");
        for sym in syms {
            let _ = writeln!(
                out,
                "  {} {} (L{}-L{})",
                sym.kind, sym.name, sym.line_start, sym.line_end,
            );
        }
        let _ = writeln!(out);
    }

    // Churn section (only if non-empty).
    if !map.churn_scores.is_empty() {
        let _ = writeln!(out, "=== Churn Scores ===\n");
        for cs in &map.churn_scores {
            let _ = writeln!(
                out,
                "  {}: score={:.2}, commits={}, lines_changed={}",
                cs.file_path, cs.normalized_score, cs.commit_count, cs.lines_changed,
            );
        }
        let _ = writeln!(out);
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    /// A small Rust source snippet used by multiple tests.
    const SAMPLE_SOURCE: &str = r#"
fn hello() {
    println!("hello");
}

struct Point {
    x: f64,
    y: f64,
}

enum Color {
    Red,
    Green,
    Blue,
}

trait Drawable {
    fn draw(&self);
}

impl Point {
    fn magnitude(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}
"#;

    #[test]
    fn test_parse_extracts_function_symbol() {
        let symbols = parse_file("test.rs", SAMPLE_SOURCE).expect("parse should succeed");

        let functions: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();

        // Should find: hello, draw (inside trait? no — trait body is not
        // walked), and magnitude (inside impl).
        let names: Vec<&str> = functions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"), "expected 'hello' in {names:?}",);
        assert!(
            names.contains(&"magnitude"),
            "expected 'magnitude' in {names:?}",
        );
    }

    #[test]
    fn test_parse_extracts_struct_symbol() {
        let symbols = parse_file("test.rs", SAMPLE_SOURCE).expect("parse should succeed");

        let structs: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Struct)
            .collect();

        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Point");
    }

    #[test]
    fn test_parse_extracts_enum_symbol() {
        let symbols = parse_file("test.rs", SAMPLE_SOURCE).expect("parse should succeed");

        let enums: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Enum)
            .collect();

        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Color");
    }

    #[test]
    fn test_parse_extracts_trait_symbol() {
        let symbols = parse_file("test.rs", SAMPLE_SOURCE).expect("parse should succeed");

        let traits: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Trait)
            .collect();

        assert_eq!(traits.len(), 1);
        assert_eq!(traits[0].name, "Drawable");
    }

    #[test]
    fn test_parse_extracts_impl_symbol() {
        let symbols = parse_file("test.rs", SAMPLE_SOURCE).expect("parse should succeed");

        let impls: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Impl)
            .collect();

        assert_eq!(impls.len(), 1);
        assert_eq!(impls[0].name, "Point");
    }

    #[test]
    fn test_symbol_line_ranges() {
        let symbols = parse_file("test.rs", SAMPLE_SOURCE).expect("parse should succeed");

        // `fn hello()` starts at line 2 of the snippet (line 1 is blank).
        let hello = symbols
            .iter()
            .find(|s| s.name == "hello")
            .expect("hello should exist");
        assert_eq!(hello.line_start, 2, "hello should start at line 2");
        assert_eq!(hello.line_end, 4, "hello should end at line 4");

        // `struct Point` starts at line 6.
        let point = symbols
            .iter()
            .find(|s| s.name == "Point" && s.kind == SymbolKind::Struct)
            .expect("Point struct should exist");
        assert_eq!(point.line_start, 6, "Point should start at line 6");
        assert_eq!(point.line_end, 9, "Point should end at line 9");
    }

    #[test]
    fn test_parse_empty_source() {
        let symbols = parse_file("empty.rs", "").expect("parsing empty source should succeed");
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_format_repo_map_output() {
        let map = RepoMap {
            symbols: vec![
                Symbol {
                    name: "main".to_string(),
                    kind: SymbolKind::Function,
                    file_path: "src/main.rs".to_string(),
                    line_start: 1,
                    line_end: 5,
                },
                Symbol {
                    name: "Config".to_string(),
                    kind: SymbolKind::Struct,
                    file_path: "src/config.rs".to_string(),
                    line_start: 3,
                    line_end: 10,
                },
            ],
            churn_scores: vec![ChurnScore {
                file_path: "src/main.rs".to_string(),
                commit_count: 12,
                lines_changed: 80,
                normalized_score: 1.0,
            }],
            indexed_file_count: 2,
            language_file_counts: HashMap::from([("rust".to_string(), 2)]),
            total_lines: 150,
        };

        let output = format_repo_map(&map);

        assert!(output.contains("2 files"), "should mention file count",);
        assert!(output.contains("150 lines"), "should mention line count",);
        assert!(output.contains("fn main"), "should list main function",);
        assert!(
            output.contains("struct Config"),
            "should list Config struct",
        );
        assert!(output.contains("Churn Scores"), "should have churn section",);
        assert!(
            output.contains("src/main.rs"),
            "should reference main.rs in churn",
        );
        assert!(
            output.contains("score=1.00"),
            "should show normalised score",
        );
    }

    #[test]
    fn test_format_repo_map_no_churn() {
        let map = RepoMap {
            symbols: vec![],
            churn_scores: vec![],
            indexed_file_count: 0,
            language_file_counts: HashMap::new(),
            total_lines: 0,
        };

        let output = format_repo_map(&map);
        assert!(
            !output.contains("Churn Scores"),
            "should omit churn section when empty",
        );
    }

    #[test]
    fn test_churn_score_fields() {
        let cs = ChurnScore {
            file_path: "lib.rs".to_string(),
            commit_count: 5,
            lines_changed: 42,
            normalized_score: 0.75,
        };
        assert_eq!(cs.commit_count, 5);
        assert_eq!(cs.lines_changed, 42);
        assert!((cs.normalized_score - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_directory_skips_unsupported_language_files_gracefully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rust_file = dir.path().join("lib.rs");
        let python_file = dir.path().join("tool.py");

        fs::write(&rust_file, "fn hello() {}\n").expect("write rust");
        fs::write(&python_file, "def hello():\n    return 1\n").expect("write py");

        let languages = RepoLanguages {
            python: true,
            ..Default::default()
        };

        let symbols = parse_directory_with_languages(&dir.path().to_string_lossy(), &languages)
            .expect("parse directory");

        assert!(symbols
            .iter()
            .any(|s| s.name == "hello" && s.file_path.ends_with("lib.rs")));
        assert!(
            !symbols.iter().any(|s| s.file_path.ends_with("tool.py")),
            "python files should be skipped because parser is unsupported"
        );
    }

    #[test]
    fn test_build_repo_map_language_counts_follow_enablement() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write a.rs");
        fs::write(dir.path().join("b.py"), "print('hi')\n").expect("write b.py");
        fs::write(dir.path().join("c.ts"), "export const x = 1;\n").expect("write c.ts");

        let languages = RepoLanguages {
            python: true,
            ..Default::default()
        };

        let map = build_repo_map_with_languages(&dir.path().to_string_lossy(), &languages)
            .expect("build repo map");

        assert_eq!(map.indexed_file_count, 2);
        assert_eq!(map.language_file_counts.get("rust"), Some(&1));
        assert_eq!(map.language_file_counts.get("python"), Some(&1));
        assert!(!map.language_file_counts.contains_key("typescript"));
    }

    #[test]
    fn test_build_repo_map_includes_typescript_symbols_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("index.ts"),
            r#"
export function add(a: number, b: number): number { return a + b; }
export class Calculator {}
interface MathOps { add(a: number, b: number): number; }
const handler = () => 42;
"#,
        )
        .expect("write index.ts");

        let languages = RepoLanguages {
            rust: false,
            python: false,
            typescript: true,
            go: false,
            java: false,
        };

        let map = build_repo_map_with_languages(&dir.path().to_string_lossy(), &languages)
            .expect("build repo map");

        assert_eq!(map.indexed_file_count, 1);
        assert_eq!(map.language_file_counts.get("typescript"), Some(&1));
        assert!(map
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Function && s.name == "add"));
        assert!(map
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Struct && s.name == "Calculator"));
        assert!(map
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Trait && s.name == "MathOps"));
        assert!(map
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Function && s.name == "handler"));
    }

    #[test]
    fn test_parse_typescript_tracks_multiline_symbols_and_ignores_false_positives() {
        let source = r#"const docs = "function fake() {}";
// class CommentOnly {}
/*
interface Hidden {}
*/
export async function loadThing<T>(
  value: T,
): Promise<T> {
  return value;
}

const handler: (
  input: string,
) => Promise<string> = async (
  input: string,
) => {
  return input;
};

const helper = function namedHelper() {
  return 1;
};

const ViewModel = class InternalViewModel {};
"#;

        let symbols = parse_typescript_file("index.ts", source);
        let summary = symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.kind.clone(),
                    symbol.line_start,
                    symbol.line_end,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            summary,
            vec![
                ("loadThing", SymbolKind::Function, 6, 10),
                ("handler", SymbolKind::Function, 12, 18),
                ("helper", SymbolKind::Function, 20, 22),
                ("ViewModel", SymbolKind::Struct, 24, 24),
            ]
        );
        assert!(
            !symbols
                .iter()
                .any(|symbol| matches!(symbol.name.as_str(), "fake" | "CommentOnly" | "Hidden")),
            "comments and string literals should not produce symbols"
        );
    }

    #[test]
    fn test_build_repo_map_extracts_typescript_symbols_from_module_extensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("view.tsx"),
            r#"const first = () => 1,
  second: () => number = () => 2,
  third = function namedThird() {
    return 3;
  },
  Widget = class Widget {};
"#,
        )
        .expect("write view.tsx");
        fs::write(
            dir.path().join("server.mts"),
            "export default function renderServer() {\n  return null;\n}\n",
        )
        .expect("write server.mts");
        fs::write(
            dir.path().join("legacy.cts"),
            "export interface LegacyShape {\n  value: string;\n}\n",
        )
        .expect("write legacy.cts");

        let languages = RepoLanguages {
            rust: false,
            python: false,
            typescript: true,
            go: false,
            java: false,
        };

        let map = build_repo_map_with_languages(&dir.path().to_string_lossy(), &languages)
            .expect("build repo map");
        let symbol_names = map
            .symbols
            .iter()
            .map(|symbol| format!("{}:{}:{}", symbol.file_path, symbol.kind, symbol.name))
            .collect::<Vec<_>>();

        assert_eq!(map.indexed_file_count, 3);
        assert_eq!(map.language_file_counts.get("typescript"), Some(&3));
        assert_eq!(
            symbol_names,
            vec![
                format!(
                    "{}:trait:LegacyShape",
                    dir.path().join("legacy.cts").to_string_lossy()
                ),
                format!(
                    "{}:fn:renderServer",
                    dir.path().join("server.mts").to_string_lossy()
                ),
                format!("{}:fn:first", dir.path().join("view.tsx").to_string_lossy()),
                format!(
                    "{}:fn:second",
                    dir.path().join("view.tsx").to_string_lossy()
                ),
                format!("{}:fn:third", dir.path().join("view.tsx").to_string_lossy()),
                format!(
                    "{}:struct:Widget",
                    dir.path().join("view.tsx").to_string_lossy()
                ),
            ]
        );
    }

    #[test]
    fn test_format_repo_map_sorts_symbols_within_each_file() {
        let map = RepoMap {
            symbols: vec![
                Symbol {
                    name: "late".to_string(),
                    kind: SymbolKind::Function,
                    file_path: "src/app.ts".to_string(),
                    line_start: 20,
                    line_end: 22,
                },
                Symbol {
                    name: "ViewModel".to_string(),
                    kind: SymbolKind::Struct,
                    file_path: "src/app.ts".to_string(),
                    line_start: 3,
                    line_end: 8,
                },
                Symbol {
                    name: "early".to_string(),
                    kind: SymbolKind::Function,
                    file_path: "src/app.ts".to_string(),
                    line_start: 10,
                    line_end: 12,
                },
            ],
            churn_scores: vec![],
            indexed_file_count: 1,
            language_file_counts: HashMap::from([("typescript".to_string(), 1)]),
            total_lines: 30,
        };

        let output = format_repo_map(&map);
        let view_model_idx = output.find("struct ViewModel").expect("ViewModel entry");
        let early_idx = output.find("fn early").expect("early entry");
        let late_idx = output.find("fn late").expect("late entry");

        assert!(view_model_idx < early_idx);
        assert!(early_idx < late_idx);
    }

    #[test]
    fn test_walk_source_files_skips_common_large_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("target")).expect("mkdir target");
        fs::write(
            dir.path().join("target").join("ignored.rs"),
            "fn ignored() {}\n",
        )
        .expect("write ignored");
        fs::write(dir.path().join("kept.rs"), "fn kept() {}\n").expect("write kept");

        let languages = RepoLanguages::default();
        let mut visited = Vec::new();
        walk_source_files(dir.path(), &languages, &mut |path, _language| {
            visited.push(path.file_name().unwrap().to_string_lossy().to_string());
            Ok(())
        })
        .expect("walk files");

        assert_eq!(visited, vec!["kept.rs".to_string()]);
    }

    #[test]
    fn test_walk_source_files_visits_only_enabled_extensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("nested")).expect("mkdir");
        fs::write(dir.path().join("nested").join("x.rs"), "fn x() {}\n").expect("write rs");
        fs::write(dir.path().join("nested").join("y.go"), "package main\n").expect("write go");

        let languages = RepoLanguages {
            rust: true,
            python: false,
            typescript: false,
            go: false,
            java: false,
        };

        let mut visited = Vec::new();
        walk_source_files(dir.path(), &languages, &mut |path, _language| {
            visited.push(path.file_name().unwrap().to_string_lossy().to_string());
            Ok(())
        })
        .expect("walk files");

        assert_eq!(visited, vec!["x.rs".to_string()]);
    }

    #[test]
    fn test_prepare_repair_workspace_uses_git_worktree_not_repo_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());

        fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
        fs::write(dir.path().join("src").join("lib.rs"), "fn original() {}\n").expect("write lib");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "init"]);

        fs::write(dir.path().join("scratch.txt"), "untracked\n").expect("write scratch");

        let workspace_root = dir
            .path()
            .join(".mercury")
            .join("worktrees")
            .join("candidate-01");

        let mut accepted = HashMap::new();
        accepted.insert(
            RepoRelativePath::new("src/lib.rs").unwrap(),
            "fn patched() {}\n".to_string(),
        );
        prepare_repair_workspace(dir.path(), &workspace_root, &accepted)
            .expect("prepare workspace");

        let git_marker =
            fs::read_to_string(workspace_root.join(".git")).expect("worktree .git file");
        assert!(
            git_marker.starts_with("gitdir: "),
            "expected linked worktree .git marker"
        );
        assert_eq!(
            fs::read_to_string(workspace_root.join("src/lib.rs")).expect("read patched file"),
            "fn patched() {}\n"
        );
        assert!(
            !workspace_root.join("scratch.txt").exists(),
            "untracked files should not appear in detached worktree snapshots"
        );
    }

    #[test]
    fn test_prepare_repair_workspace_falls_back_when_not_git_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
        fs::write(dir.path().join("src").join("lib.rs"), "fn fallback() {}\n").expect("write lib");

        let workspace_root = dir
            .path()
            .join(".mercury")
            .join("worktrees")
            .join("fallback");
        let accepted = HashMap::new();
        prepare_repair_workspace(dir.path(), &workspace_root, &accepted)
            .expect("prepare workspace");

        assert!(workspace_root.join("src/lib.rs").exists());
        assert!(
            !workspace_root.join(".git").exists(),
            "fallback copy mode should not synthesize git metadata"
        );
    }

    #[test]
    fn test_repo_relative_path_rejects_parent_traversal() {
        let err = RepoRelativePath::new("../../etc/passwd").expect_err("path should be rejected");
        assert!(matches!(err, RepoError::InvalidRepoRelativePath { .. }));
        assert!(err.to_string().contains("parent-directory traversal"));
    }

    #[test]
    fn test_repo_relative_path_rejects_absolute_path() {
        let err = RepoRelativePath::new("/tmp/file").expect_err("path should be rejected");
        assert!(matches!(err, RepoError::InvalidRepoRelativePath { .. }));
        assert!(err.to_string().contains("absolute paths are not allowed"));
    }

    #[test]
    fn test_repo_relative_path_rejects_windows_absolute_path() {
        let err = RepoRelativePath::new(r"C:\temp\file.rs").expect_err("path should be rejected");
        assert!(matches!(err, RepoError::InvalidRepoRelativePath { .. }));
        assert!(err
            .to_string()
            .contains("absolute Windows paths are not allowed"));
    }

    #[test]
    #[cfg(unix)]
    fn test_repo_relative_path_rejects_symlink_escape_targets() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().expect("repo tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        symlink(outside.path(), repo.path().join("escape")).expect("create escape symlink");

        let err = RepoRelativePath::from_planner_path(repo.path(), "escape/file.rs")
            .expect_err("symlink escape should be rejected");
        assert!(matches!(err, RepoError::InvalidRepoRelativePath { .. }));
        assert!(err
            .to_string()
            .contains("symlink escapes the repository root"));
    }

    fn init_git_repo(path: &Path) {
        run_git(path, &["init", "-q"]);
        run_git(path, &["config", "user.email", "mercury@example.com"]);
        run_git(path, &["config", "user.name", "Mercury Tests"]);
    }

    fn run_git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Mercury Tests")
            .env("GIT_AUTHOR_EMAIL", "mercury@example.com")
            .env("GIT_COMMITTER_NAME", "Mercury Tests")
            .env("GIT_COMMITTER_EMAIL", "mercury@example.com")
            .output()
            .expect("git command should run");

        assert!(
            output.status.success(),
            "git command failed: git -C {} {} stderr={}",
            path.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
