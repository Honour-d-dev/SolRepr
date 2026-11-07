use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use lsp_types::Position;
use lsp_types::Url;
use ropey::Rope;
use tree_sitter::Point;

use crate::workspace::Remapping;

pub(crate) fn canonicalize_path(path: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let path = dunce::canonicalize(path).context("failed to canonicalize path")?;
    Utf8PathBuf::from_path_buf(path)
        .map_err(|path| anyhow::anyhow!("path is not valid UTF-8: {path:?}"))
}

#[cfg(test)]
pub(crate) fn fixture_path(path: &str) -> Utf8PathBuf {
    let path = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    canonicalize_path(&path).expect("fixture exists")
}

pub(crate) fn normalize_path(path: &Utf8Path) -> Utf8PathBuf {
    if let Ok(path) = canonicalize_path(path) {
        return path;
    }

    // canonicalizing can fail if file has not been saved to disk.
    // In this case, we can still try to canonicalize the parent path then join the file name.
    let (Some(file_name), Some(parent)) = (path.file_name(), path.parent()) else {
        return path.to_path_buf();
    };

    let normalized_parent = canonicalize_path(parent).unwrap_or_else(|_| parent.to_path_buf());
    normalized_parent.join(file_name)
}

pub(crate) fn to_utf8path(uri: &Url) -> anyhow::Result<Utf8PathBuf> {
    let path = uri
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("failed to convert URI to path"))?;
    let path =
        Utf8PathBuf::from_path_buf(path).map_err(|_| anyhow::anyhow!("path is not valid UTF-8"))?;
    Ok(normalize_path(&path))
}

pub(crate) fn to_url(path: &Utf8Path) -> Url {
    Url::from_file_path(path).expect("failed to convert path to url")
}

/// A document's path, or `None` when the client used a non-`file:` URI.
/// Notifications have no reply, so this only logs.
pub(crate) fn document_path(uri: &lsp_types::Url) -> Option<Utf8PathBuf> {
    match to_utf8path(uri) {
        Ok(path) => Some(path),
        Err(error) => {
            log_info(format!("ignoring non-file document URI {uri}: {error:#}"));
            None
        }
    }
}

///Resolves to a new path relative to a base path
pub(crate) fn resolve_path(
    base: &Utf8PathBuf,
    rel_path: impl AsRef<Utf8Path>,
) -> anyhow::Result<Utf8PathBuf> {
    let resolved_path = base
        .parent()
        .context("Error: can't resolve path, base path has no parent")?
        .join(rel_path);
    Ok(normalize_path(&resolved_path))
}

/// Resolves a Solidity import string, checking remappings first, then falling back to relative resolution.
pub(crate) fn resolve_import(
    base: &Utf8PathBuf,
    import_str: &str,
    remappings: &[Remapping],
) -> anyhow::Result<Utf8PathBuf> {
    // Relative imports
    if import_str.starts_with("./") || import_str.starts_with("../") {
        return resolve_path(base, import_str);
    }

    let best_match = remappings
        .iter()
        .filter(|remapping| {
            import_str.starts_with(&remapping.prefix)
                && remapping
                    .context
                    .as_ref()
                    .is_none_or(|context| base.starts_with(context))
        })
        .max_by_key(|remapping| {
            (
                remapping.context.as_ref().map_or(0, |context| context.as_str().len()),
                remapping.prefix.len(),
            )
        });

    if let Some(remapping) = best_match {
        let suffix = &import_str[remapping.prefix.len()..];
        return Ok(normalize_path(&remapping.target.join(suffix)));
    }

    // Fallback: try relative resolution against the base file's parent
    resolve_path(base, import_str)
}

#[inline]
pub(crate) fn to_rope_idx(file: &Rope, pos: Position) -> usize {
    file.line_to_char(pos.line as usize)
        + file
            .line(pos.line as usize)
            .utf16_cu_to_char(pos.character as usize)
}

#[inline]
#[allow(unused)]
pub(crate) fn to_rope_byte_idx(file: &Rope, pos: Position) -> usize {
    let l = file.line(pos.line as usize);
    let c = l.utf16_cu_to_char(pos.character as usize);
    file.line_to_byte(pos.line as usize) + l.char_to_byte(c)
}

#[inline]
pub(crate) fn byte_to_position(rope: &Rope, byte: usize) -> Position {
    let byte = byte.min(rope.len_bytes());
    let line = rope.byte_to_line(byte);
    let char_index = rope.byte_to_char(byte) - rope.line_to_char(line);
    let character = rope.line(line).char_to_utf16_cu(char_index);
    Position {
        line: line as u32,
        character: character as u32,
    }
}

#[inline]
#[allow(unused)]
pub(crate) fn position_to_point(r: &Rope, position: Position) -> Point {
    Point {
        row: position.line as usize,
        column: r
        .line(position.line as usize)
        .utf16_cu_to_char(position.character as usize),
    }
}

#[inline]
#[allow(unused)]
pub(crate) fn char_to_point(r: &Rope, char_idx: usize) -> Point {
    //assumes shar index is in unicode code points not utf16 code units
    let ctl = r.char_to_line(char_idx);
    Point {
        row: ctl,
        column: char_idx - r.line_to_char(ctl),
    }
}

#[inline]
#[allow(unused)]
pub(crate) fn byte_to_point(r: &Rope, byte_idx: usize) -> Point {
    let ctl = r.byte_to_line(byte_idx);
    Point {
        row: ctl,
        column: byte_idx - r.line_to_byte(ctl),
    }
}

pub(crate) fn print_tree(
    cursor: &mut tree_sitter::TreeCursor,
    source_code: &str,
    indentation: usize,
) -> String {
    let indent = "  ".repeat(indentation);
    let kind = cursor.node().kind();
    let text = &source_code[cursor.node().byte_range()].replace('\n', &format!("\n {indent} "));

    let mut result = format!("\n{indent}{kind}: \"{text}\""); //add field name and id if present so wee see what its about

    iterate_children!(cursor, {
        result.push_str(&print_tree(cursor, source_code, indentation + 2));
    });
    result
}

pub(crate) fn print_named_tree(
    cursor: &mut tree_sitter::TreeCursor,
    source_code: &str,
    indentation: usize,
) -> String {
    if !cursor.node().is_named() {
        return "".to_string();
    }
    let indent = "  ".repeat(indentation); //todo for only named nodes
    let kind = cursor.node().kind();

    let text = &source_code[cursor.node().byte_range()];
    // dont  print bodies, blocks and statements
    let mut result = if text.len() > 120 || kind == "statement" || text.contains("{") {
        format!("\n{indent}{kind}:")
    } else {
        let text = text.replace('\n', &format!("\n {indent} "));
        format!("\n{indent}{kind}: \"{text}\"") //add field name and id if present so wee see what its about
    };

    iterate_children!(cursor, {
        result.push_str(&print_named_tree(cursor, source_code, indentation + 2));
    });
    result
}

pub(crate) fn log_info(message: impl Into<String>) {
    tracing::info!("{}", message.into());
}

/**
 * Iterates over the children of the current node using a cursor
 * returns cursor back to parent node afterwards
 */
macro_rules! iterate_children {
    ($cursor:expr, $body:block) => {
        if $cursor.goto_first_child() {
            loop {
                $body
                if !$cursor.goto_next_sibling() {
                    break;
                }
            }
            $cursor.goto_parent();
        }
    };

    ($cursor:expr, $prefix:expr, $body:block) => {
        if $cursor.goto_first_child() {
            $prefix;
            loop {
                $body
                if !$cursor.goto_next_sibling() {
                    break;
                }
            }
            $cursor.goto_parent();
        }
    };
}

pub(crate) use iterate_children;

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn test_print_tree() {
        //To run test:
        //TESTPATH=/path/to/sol/file.sol cargo test test_print_tree_counter_user

        let source = r#"// SPDX-License-Identifier: UNLICENSED
        pragma solidity ^0.8.30;

        contract A is B {

            function testD(string memory s) external returns (string storage, bytes memory b) {
               this.
            }

            function functionPointer() external view returns (uint256) {
                function(uint256) internal pure returns (uint256) fn = this.identity;
                return fn(1);
            }

            function identity(uint256 value) external pure returns (uint256) {
                return value;
            }
        }"#;

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_solidity::LANGUAGE.into())
            .expect("failed to set language");
        let tree = parser.parse(source, None).expect("failed to parse");

        let output = print_named_tree(&mut tree.root_node().walk(), source, 0);
        assert!(!output.is_empty());
        println!("{}", output);
    }

    #[test]
    fn test_print_completion_context_trees() {
        let cases = [
            ("this_dot", "contract A { function f() { this.token. } }"),
            ("msg_dot", "contract A { function f() { msg. } }"),
            (
                "token_dot",
                "contract A { function f(IERC20 token) { token. } }",
            ),
            (
                "cast_dot",
                "contract A { function f(address token) { token.IERC20(token)..some } }",
            ),
            (
                "emit",
                "contract A { event E(); function f() { emit E(); } }",
            ),
            (
                "emit_incomplete",
                "contract A { event E(); function f() { emit } }",
            ),
            (
                "revert",
                "contract A { error E(); function f() { revert E(); } }",
            ),
            (
                "revert_incomplete",
                "contract A { error E(); function f() { revert } }",
            ),
            (
                "type",
                "contract A { function f() public returns (uint256) { return 0; } }",
            ),
            (
                "type_incomplete",
                "contract A { function f() public returns ( { return 0; } }",
            ),
            (
                "expression",
                "contract A { function f(uint256 value) { value; } }",
            ),
            (
                "expression_incomplete",
                "contract A { function f(uint256 value) { value + } }",
            ),
            (
                "call_incomplete",
                "contract A { function f(uint256 value) { foo( } }",
            ),
            ("item_list", "contract A { }"),
        ];

        for (name, source) in cases {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_solidity::LANGUAGE.into())
                .expect("failed to set language");
            let tree = parser.parse(source, None).expect("failed to parse");
            let output = print_named_tree(&mut tree.root_node().walk(), source, 0);
            assert!(!output.is_empty(), "empty AST for {name}");
            println!("--- {name} ---\n{output}");
        }
    }

    #[test]
    fn context_remapping_wins_only_for_importers_in_its_context() {
        let root_remapping = Remapping {
            context: None,
            prefix: "@openzeppelin/contracts/".to_string(),
            target: Utf8PathBuf::from("/workspace/lib/openzeppelin-contracts/contracts"),
        };
        let nested_remapping = Remapping {
            context: Some(Utf8PathBuf::from(
                "/workspace/lib/openzeppelin-contracts-upgradeable",
            )),
            prefix: "@openzeppelin/contracts/".to_string(),
            target: Utf8PathBuf::from(
                "/workspace/lib/openzeppelin-contracts-upgradeable/lib/openzeppelin-contracts/contracts",
            ),
        };
        let remappings = [root_remapping, nested_remapping];
        let import = "@openzeppelin/contracts/token/ERC721/IERC721.sol";

        let nested = resolve_import(
            &Utf8PathBuf::from(
                "/workspace/lib/openzeppelin-contracts-upgradeable/contracts/ERC721.sol",
            ),
            import,
            &remappings,
        )
        .unwrap();
        assert_eq!(
            nested,
            Utf8PathBuf::from(
                "/workspace/lib/openzeppelin-contracts-upgradeable/lib/openzeppelin-contracts/contracts/token/ERC721/IERC721.sol"
            )
        );

        let root = resolve_import(
            &Utf8PathBuf::from("/workspace/src/MyNFT.sol"),
            import,
            &remappings,
        )
        .unwrap();
        assert_eq!(
            root,
            Utf8PathBuf::from(
                "/workspace/lib/openzeppelin-contracts/contracts/token/ERC721/IERC721.sol"
            )
        );
    }
}
