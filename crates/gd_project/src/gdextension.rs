//! `.gdextension` enumeration.
//!
//! A `.gdextension` file is config only (`[configuration]` `entry_symbol`, `[libraries]` per-platform
//! paths, optional `[icons]`); it carries no class API. We use it to *locate* installed extensions
//! and their `doc_classes` XML — the stock `extension_api.json` dump omits GDExtensions, so their
//! types come from the XML reader (`gd_types::doc_xml`). The `[icons]` section lists class names,
//! used only as a hint for whether to emit a "no docs found" notice.

use camino::{Utf8Path, Utf8PathBuf};
use walkdir::WalkDir;

/// An installed GDExtension discovered by scanning `res://**/*.gdextension`.
#[derive(Clone, Debug)]
pub struct GdExtension {
    /// Path to the `.gdextension` config file.
    pub config: Utf8PathBuf,
    /// The directory the config lives in (the addon root).
    pub addon_dir: Utf8PathBuf,
    /// Class names hinted by the `[icons]` section (not authoritative — a hint only).
    pub class_hints: Vec<String>,
}

impl GdExtension {
    /// Every `*.xml` file in a `doc_classes/` directory under this extension's addon directory
    /// (skipping the `.godot/` cache). These are the fallback source for GDExtension class APIs,
    /// which the stock `extension_api.json` dump omits (`docs/03` §2). An addon shipping no docs
    /// simply yields none.
    ///
    /// The `doc_classes/` parent is the same test the watcher uses to decide that an XML change
    /// reloads the native DB (`gd_server::watcher`). Merging any XML the watcher does not react to
    /// would let a stray `<class>` file (a build artifact, an engine doc copied under the addon) slip
    /// in on some unrelated reload and then outlive its own deletion until restart.
    pub fn doc_xml_files(&self) -> Vec<Utf8PathBuf> {
        WalkDir::new(&self.addon_dir)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".godot")
            .flatten()
            .filter_map(|e| Utf8Path::from_path(e.path()).map(Utf8Path::to_path_buf))
            .filter(|p| p.extension() == Some("xml") && is_doc_classes_xml(p))
            .collect()
    }
}

/// Whether `path` sits directly in a `doc_classes/` directory (any case), the layout GDExtensions
/// ship their class docs in.
#[must_use]
pub fn is_doc_classes_xml(path: &Utf8Path) -> bool {
    path.parent()
        .and_then(Utf8Path::file_name)
        .is_some_and(|n| n.eq_ignore_ascii_case("doc_classes"))
}

/// Find every `.gdextension` under `root` (excluding the `.godot/` import cache).
pub fn enumerate(root: &Utf8Path) -> Vec<GdExtension> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".godot")
        .flatten()
    {
        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue;
        };
        if path.extension() != Some("gdextension") {
            continue;
        }
        let class_hints = std::fs::read_to_string(path)
            .map(|t| icon_class_hints(&t))
            .unwrap_or_default();
        let addon_dir = path
            .parent()
            .map(Utf8Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        out.push(GdExtension {
            config: path.to_path_buf(),
            addon_dir,
            class_hints,
        });
    }
    out
}

/// The keys of the `[icons]` section are class names (`Terrain3D = "res://…/terrain3d.svg"`).
fn icon_class_hints(text: &str) -> Vec<String> {
    let mut hints = Vec::new();
    let mut in_icons = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(section) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_icons = section.trim() == "icons";
            continue;
        }
        if in_icons {
            if let Some((key, _)) = t.split_once('=') {
                let key = key.trim();
                if !key.is_empty() {
                    hints.push(key.to_owned());
                }
            }
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_hints_from_section() {
        let text = "[configuration]\nentry_symbol = \"x_init\"\n[icons]\nTerrain3D = \"res://a.svg\"\nTerrain3DMesh = \"res://b.svg\"\n";
        assert_eq!(icon_class_hints(text), vec!["Terrain3D", "Terrain3DMesh"]);
    }

    /// Only `doc_classes/` XML is merged, the same set the watcher reloads on, so a stray `<class>`
    /// file elsewhere in the addon can never enter the DB on an unrelated reload (#622).
    #[test]
    fn doc_xml_files_reads_only_doc_classes() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        for rel in [
            "doc_classes/A.xml",
            "Doc_Classes/B.xml",
            "build/Vector3.xml",
            "doc_classes/nested/C.xml",
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "<class name=\"X\"/>").unwrap();
        }
        let ext = GdExtension {
            config: root.join("x.gdextension"),
            addon_dir: root.to_path_buf(),
            class_hints: Vec::new(),
        };
        let mut got: Vec<String> = ext
            .doc_xml_files()
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().as_str().replace('\\', "/"))
            .collect();
        got.sort();
        assert_eq!(got, ["Doc_Classes/B.xml", "doc_classes/A.xml"]);
    }
}
