//! Repository analysis — tree-sitter parsing, symbol extraction, and git churn scoring.
//!
//! This module provides a [`RepoMap`] that summarises every symbol and churn
//! hotspot in a codebase. The map is designed to be compact enough to fit
//! inside Mercury 2's context window while still giving the model a useful
//! structural overview of the code it is about to edit.

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

use git2::Repository;
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
}

/// Convenience alias used throughout this module.
pub type Result<T> = std::result::Result<T, RepoError>;

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

/// A single code symbol extracted from a Rust source file.
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
    /// All symbols extracted from Rust source files.
    pub symbols: Vec<Symbol>,
    /// Per-file git churn scores.
    pub churn_scores: Vec<ChurnScore>,
    /// Number of `.rs` files discovered.
    pub file_count: usize,
    /// Total line count across all `.rs` files.
    pub total_lines: usize,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a single Rust source string and return extracted symbols.
///
/// `path` is recorded verbatim in each [`Symbol::file_path`] field; it does
/// not need to point to a real file on disk.
pub fn parse_file(path: &str, source: &str) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|_| RepoError::ParseFailed(path.to_string()))?;

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

/// Recursively walk `dir_path`, parse every `.rs` file, and return all symbols.
pub fn parse_directory(dir_path: &str) -> Result<Vec<Symbol>> {
    let mut all_symbols = Vec::new();
    walk_rs_files(Path::new(dir_path), &mut |path| {
        let source = fs::read_to_string(path)?;
        let path_str = path.to_string_lossy().to_string();
        let syms = parse_file(&path_str, &source)?;
        all_symbols.extend(syms);
        Ok(())
    })?;
    Ok(all_symbols)
}

/// Internal helper that recursively visits every `.rs` file under `dir`.
fn walk_rs_files(dir: &Path, visitor: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_rs_files(&path, visitor)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            visitor(&path)?;
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
    let symbols = parse_directory(dir_path)?;

    // Count files and total lines.
    let mut file_count: usize = 0;
    let mut total_lines: usize = 0;
    walk_rs_files(Path::new(dir_path), &mut |path| {
        file_count += 1;
        let content = fs::read_to_string(path)?;
        total_lines += content.lines().count();
        Ok(())
    })?;

    // Git churn is best-effort; a missing `.git` folder should not be fatal.
    let churn_scores = git_churn_scores(dir_path).unwrap_or_default();

    Ok(RepoMap {
        symbols,
        churn_scores,
        file_count,
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
        map.file_count, map.total_lines,
    );

    // Group symbols by file.
    let mut by_file: HashMap<&str, Vec<&Symbol>> = HashMap::new();
    for sym in &map.symbols {
        by_file.entry(&sym.file_path).or_default().push(sym);
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
            file_count: 2,
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
            file_count: 0,
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
}
