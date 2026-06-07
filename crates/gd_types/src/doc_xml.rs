//! Reader for `doc_classes` / class-reference XML — the static fallback for GDExtension types, and
//! for engine `doc/classes/*.xml`. One reader serves both.
//!
//! The XML encodes types differently than the JSON dump: a typed array is a `"X[]"` suffix and an
//! enum/bitfield is a separate `enum=` attribute (`+ is_bitfield`). We **normalize** those back into
//! the dump's prefix form (`"typedarray::X"`, `"enum::X"`, `"bitfield::X"`) and emit an
//! [`api::ClassDef`], so [`crate::native_db`] ingestion — and the [`crate::type_ref`] decoder — are
//! reused verbatim. Output is tagged `api_type = "extension"`; it is only ever merged as a fallback
//! (the JSON dump wins on conflict — see [`crate::native_db::NativeDb::merge_doc_class`]).

use roxmltree::Node;

use crate::api::{
    ArgumentDef, ClassConstant, ClassDef, EnumDef, EnumValue, MethodDef, PropertyDef, SignalDef,
    ValueType,
};

#[derive(Debug, thiserror::Error)]
pub enum DocXmlError {
    #[error("could not read doc XML at {0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("invalid doc XML: {0}")]
    Xml(#[from] roxmltree::Error),
    #[error("root element is <{0}>, expected <class>")]
    NotAClass(String),
    #[error("<class> is missing its name attribute")]
    MissingName,
}

/// Parse one class-reference XML document into an [`api::ClassDef`] (`api_type = "extension"`).
pub fn parse_class(xml: &str) -> Result<ClassDef, DocXmlError> {
    let doc = roxmltree::Document::parse(xml)?;
    let root = doc.root_element();
    let tag = root.tag_name().name();
    if tag != "class" {
        return Err(DocXmlError::NotAClass(tag.to_owned()));
    }
    let name = root
        .attribute("name")
        .ok_or(DocXmlError::MissingName)?
        .to_owned();
    let inherits = root.attribute("inherits").map(str::to_owned);

    let mut methods = Vec::new();
    let mut properties = Vec::new();
    let mut signals = Vec::new();
    let mut constants = Vec::new();
    let mut enums: Vec<EnumDef> = Vec::new();

    for section in root.children().filter(Node::is_element) {
        match section.tag_name().name() {
            "methods" | "constructors" => {
                methods.extend(elements(section, "method").map(parse_method));
            }
            "members" => {
                properties.extend(elements(section, "member").map(parse_member));
            }
            "signals" => {
                signals.extend(elements(section, "signal").map(parse_signal));
            }
            "constants" => {
                for c in elements(section, "constant") {
                    let cname = attr(c, "name").to_owned();
                    let value = attr(c, "value").parse::<i64>().unwrap_or(0);
                    match c.attribute("enum") {
                        // Doc XML models enums as constants grouped by an `enum=` attribute.
                        Some(en) => push_enum_value(&mut enums, en, cname, value),
                        None => constants.push(ClassConstant { name: cname, value }),
                    }
                }
            }
            _ => {}
        }
    }

    let brief_description = first_child_text(root, "brief_description");
    let description = first_child_text(root, "description");

    Ok(ClassDef {
        name,
        inherits,
        is_refcounted: false,
        is_instantiable: true,
        api_type: "extension".to_owned(),
        methods,
        properties,
        signals,
        enums,
        constants,
        brief_description,
        description,
    })
}

/// Parse a class-reference XML file from disk.
pub fn parse_file(path: &str) -> Result<ClassDef, DocXmlError> {
    let text = std::fs::read_to_string(path).map_err(|e| DocXmlError::Io(path.to_owned(), e))?;
    parse_class(&text)
}

fn parse_method(m: Node) -> MethodDef {
    let qualifiers = m.attribute("qualifiers").unwrap_or("");
    let has = |flag: &str| qualifiers.split_whitespace().any(|w| w == flag);
    let return_value = m
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "return")
        .map(|r| ValueType {
            ty: normalized_type(r),
        })
        .filter(|v| v.ty != "void");
    MethodDef {
        name: attr(m, "name").to_owned(),
        is_const: has("const"),
        is_static: has("static"),
        is_vararg: has("vararg"),
        is_virtual: has("virtual"),
        return_value,
        arguments: params(m),
        description: first_child_text(m, "description"),
    }
}

fn parse_member(m: Node) -> PropertyDef {
    PropertyDef {
        name: attr(m, "name").to_owned(),
        ty: normalized_type(m),
        setter: m.attribute("setter").unwrap_or("").to_owned(),
        getter: m.attribute("getter").unwrap_or("").to_owned(),
        description: member_text(m),
    }
}

fn parse_signal(s: Node) -> SignalDef {
    SignalDef {
        name: attr(s, "name").to_owned(),
        arguments: params(s),
        description: first_child_text(s, "description"),
    }
}

/// Class-reference XML pattern: long-form docs are the **text content** of dedicated child elements
/// (`<description>…</description>`, `<brief_description>…</brief_description>`,
/// `<member>…</member>`). Concatenate every descendant text node so embedded markup (`<code>`,
/// `<b>`, paragraph breaks) flows through as one trimmed string. Trimming drops the doc-XML
/// convention of indenting the inner text by a tab, keeping the result hover-friendly.
fn first_child_text(parent: Node, tag: &str) -> String {
    parent
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == tag)
        .map(node_text)
        .unwrap_or_default()
}

/// `<member>` carries its description as its own text content (no `<description>` child), since
/// each member is a one-liner in the doc reference.
fn member_text(m: Node) -> String {
    node_text(m)
}

fn node_text(n: Node) -> String {
    // `n.descendants()` yields both element and text nodes. An element's `.text()` returns its
    // first text-child's content, so we'd double-count: once via the element, once via the text
    // node beneath it. Filter to text-only descendants (`is_text()`) so each character is
    // collected exactly once.
    let mut buf = String::new();
    for desc in n.descendants() {
        if desc.is_text() {
            if let Some(t) = desc.text() {
                buf.push_str(t);
            }
        }
    }
    buf.trim().to_owned()
}

fn params(parent: Node) -> Vec<ArgumentDef> {
    elements(parent, "param")
        .map(|p| ArgumentDef {
            name: attr(p, "name").to_owned(),
            ty: normalized_type(p),
            default_value: p.attribute("default").map(str::to_owned),
        })
        .collect()
}

/// Normalize an element's `type` / `enum` / `is_bitfield` attributes into the dump's prefix encoding,
/// so the shared decoder handles it.
fn normalized_type(n: Node) -> String {
    if let Some(en) = n.attribute("enum") {
        let kind = if n.attribute("is_bitfield") == Some("true") {
            "bitfield"
        } else {
            "enum"
        };
        return format!("{kind}::{en}");
    }
    let ty = n.attribute("type").unwrap_or("Variant");
    match ty.strip_suffix("[]") {
        Some(elem) => format!("typedarray::{elem}"),
        None => ty.to_owned(),
    }
}

fn push_enum_value(enums: &mut Vec<EnumDef>, enum_name: &str, value_name: String, value: i64) {
    let entry = EnumValue {
        name: value_name,
        value,
    };
    if let Some(e) = enums.iter_mut().find(|e| e.name == enum_name) {
        e.values.push(entry);
    } else {
        enums.push(EnumDef {
            name: enum_name.to_owned(),
            is_bitfield: false,
            values: vec![entry],
        });
    }
}

fn elements<'a, 'i>(parent: Node<'a, 'i>, tag: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    parent
        .children()
        .filter(move |n| n.is_element() && n.tag_name().name() == tag)
}

fn attr<'a>(n: Node<'a, '_>, key: &str) -> &'a str {
    n.attribute(key).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Doc XML format mirrors `doc/classes/<Class>.xml` in the Godot tree. Class-level
    /// `<brief_description>` + `<description>` flow into `ClassDef.brief_description` /
    /// `.description`; per-method `<description>` flows into `MethodDef.description`. WP-H's
    /// hover renderer reads these strings — this test pins the extraction so a future XML
    /// schema change doesn't silently strip the docs out.
    #[test]
    fn class_and_method_descriptions_extract_from_doc_xml() {
        let xml = r#"
<class name="Foo">
    <brief_description>A test class.</brief_description>
    <description>The long-form description of [Foo].</description>
    <methods>
        <method name="bar">
            <return type="int" />
            <description>Returns the bar.</description>
        </method>
    </methods>
    <members>
        <member name="x" type="int">An x member.</member>
    </members>
    <signals>
        <signal name="ping"><description>Fired on ping.</description></signal>
    </signals>
</class>"#;
        let class = parse_class(xml).expect("parses");
        assert_eq!(class.brief_description, "A test class.");
        assert_eq!(class.description, "The long-form description of [Foo].");
        assert_eq!(class.methods[0].description, "Returns the bar.");
        assert_eq!(class.properties[0].description, "An x member.");
        assert_eq!(class.signals[0].description, "Fired on ping.");
    }
}
