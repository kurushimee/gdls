//! Virtual file system: an in-memory overlay of open editor buffers.
//!
//! The source of truth for a `.gd` file is its open buffer if one is open, else the file on disk.
//! M0 tracks open buffers (driven by `didOpen`/`didChange`/`didClose`); on-demand disk reads for
//! closed files arrive with the project indexer in M2. Buffers use [`ropey`] for cheap incremental
//! edits.

use std::collections::HashMap;

use lsp_types::TextDocumentContentChangeEvent;
use ropey::Rope;

use crate::position::{PositionEncoding, PositionMapper};

/// An open document buffer.
pub struct Document {
    pub rope: Rope,
    pub version: i32,
    pub open: bool,
}

impl Document {
    /// The full text of the document.
    pub fn text(&self) -> String {
        self.rope.to_string()
    }
}

/// The open-buffer overlay, keyed by document URI string.
#[derive(Default)]
pub struct Vfs {
    docs: HashMap<String, Document>,
}

impl Vfs {
    /// Handle `didOpen`: the overlay becomes authoritative for this URI.
    pub fn open(&mut self, uri: String, text: String, version: i32) {
        self.docs.insert(
            uri,
            Document {
                rope: Rope::from_str(&text),
                version,
                open: true,
            },
        );
    }

    /// Handle `didChange`: apply incremental (or full) content changes to the buffer.
    ///
    /// All edits are funnelled through [`PositionMapper`]: an LSP `Position` is converted to a byte
    /// offset and then to a rope char index before `remove`/`insert`, so the rope is never indexed
    /// directly with an encoding-dependent client offset.
    pub fn apply_changes(
        &mut self,
        uri: &str,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        enc: PositionEncoding,
    ) {
        let Some(doc) = self.docs.get_mut(uri) else {
            return;
        };
        for change in changes {
            match change.range {
                Some(range) => {
                    let (start_byte, end_byte) = {
                        let mapper = PositionMapper::new(&doc.rope, enc);
                        (
                            mapper.position_to_byte(range.start),
                            mapper.position_to_byte(range.end),
                        )
                    };
                    let start_char = doc.rope.byte_to_char(start_byte);
                    let end_char = doc.rope.byte_to_char(end_byte.max(start_byte));
                    doc.rope.remove(start_char..end_char);
                    doc.rope.insert(start_char, &change.text);
                }
                None => doc.rope = Rope::from_str(&change.text),
            }
        }
        doc.version = version;
    }

    /// Handle `didClose`: drop the overlay (closed files fall back to disk in M2).
    pub fn close(&mut self, uri: &str) {
        self.docs.remove(uri);
    }

    /// Look up an open document.
    pub fn get(&self, uri: &str) -> Option<&Document> {
        self.docs.get(uri)
    }

    /// Iterate the URIs of every currently-open document. Used by M4's `handle_watcher` after a
    /// file changes on disk to find which open buffers need a diagnostic refresh because the
    /// changed file is a transitive dependency of theirs.
    pub fn open_uris(&self) -> impl Iterator<Item = &str> {
        self.docs.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn change(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range,
            range_length: None,
            text: text.to_string(),
        }
    }

    #[test]
    fn incremental_edit_applies() {
        let mut vfs = Vfs::default();
        vfs.open("res://a.gd".into(), "extends Node\n".into(), 1);
        // Replace "Node" with "Node2D".
        vfs.apply_changes(
            "res://a.gd",
            vec![change(
                Some(Range {
                    start: Position {
                        line: 0,
                        character: 8,
                    },
                    end: Position {
                        line: 0,
                        character: 12,
                    },
                }),
                "Node2D",
            )],
            2,
            PositionEncoding::Utf16,
        );
        assert_eq!(vfs.get("res://a.gd").unwrap().text(), "extends Node2D\n");
        assert_eq!(vfs.get("res://a.gd").unwrap().version, 2);
    }

    #[test]
    fn full_replace_and_close() {
        let mut vfs = Vfs::default();
        vfs.open("res://a.gd".into(), "old".into(), 1);
        vfs.apply_changes(
            "res://a.gd",
            vec![change(None, "new")],
            2,
            PositionEncoding::Utf16,
        );
        assert_eq!(vfs.get("res://a.gd").unwrap().text(), "new");
        vfs.close("res://a.gd");
        assert!(vfs.get("res://a.gd").is_none());
    }
}
