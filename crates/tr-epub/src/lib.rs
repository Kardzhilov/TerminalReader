//! EPUB container, OPF, spine, and simple XHTML block parsing for `TerminalReader`.

use std::{
    collections::HashMap,
    fs::File,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

use quick_xml::{Reader, events::Event};
use thiserror::Error;
use zip::ZipArchive;

#[derive(Debug, Error)]
pub enum EpubError {
    #[error("could not open EPUB: {0}")]
    Open(#[from] std::io::Error),
    #[error("invalid EPUB archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid EPUB XML: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("invalid EPUB text encoding: {0}")]
    Encoding(#[from] quick_xml::encoding::EncodingError),
    #[error("EPUB is missing {0}")]
    Missing(&'static str),
    #[error("EPUB text is not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookMetadata {
    pub title: String,
    pub authors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpineItem {
    pub path: String,
    pub linear: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    pub label: String,
    pub spine_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePathStep {
    pub name: String,
    pub ordinal: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(String),
    Heading { level: u8, text: String },
    Quote(String),
    Code(String),
    Rule,
    Image {
        alt: Option<String>,
        href: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcedBlock {
    pub block: Block,
    pub source_path: Vec<SourcePathStep>,
}

#[derive(Debug)]
pub struct EpubBook {
    archive: ZipArchive<File>,
    pub metadata: BookMetadata,
    pub spine: Vec<SpineItem>,
    pub toc: Vec<TocEntry>,
}

impl EpubBook {
    pub fn open(path: &Path) -> Result<Self, EpubError> {
        let file = File::open(path)?;
        let mut archive = ZipArchive::new(file)?;
        let container = read_entry(&mut archive, "META-INF/container.xml")?;
        let opf_path = parse_rootfile(&container)?.ok_or(EpubError::Missing("OPF rootfile"))?;
        let opf = read_entry(&mut archive, &opf_path)?;
        let parsed = parse_opf(&opf, &opf_path)?;
        let mut toc = if let Some(nav_path) = &parsed.nav_path {
            let nav = read_entry(&mut archive, nav_path)?;
            parse_nav(&nav, nav_path, &parsed.spine)?
        } else {
            Vec::new()
        };
        // EPUB 2 fallback: books without a nav document ship an NCX instead.
        if toc.is_empty() {
            if let Some(ncx_path) = &parsed.ncx_path {
                if let Ok(ncx) = read_entry(&mut archive, ncx_path) {
                    toc = parse_ncx(&ncx, ncx_path, &parsed.spine)?;
                }
            }
        }

        Ok(Self {
            archive,
            metadata: parsed.metadata,
            spine: parsed.spine,
            toc,
        })
    }

    pub fn chapter_blocks(&mut self, index: usize) -> Result<Vec<SourcedBlock>, EpubError> {
        let path = self
            .spine
            .get(index)
            .ok_or(EpubError::Missing("spine item"))?
            .path
            .clone();
        let chapter = read_entry(&mut self.archive, &path)?;
        Ok(parse_chapter(&chapter))
    }

    /// Read a resource referenced from a chapter (e.g. an image `src`),
    /// returning its archive path and raw bytes.
    pub fn resource_bytes(
        &mut self,
        chapter_index: usize,
        href: &str,
    ) -> Result<(String, Vec<u8>), EpubError> {
        let chapter_path = self
            .spine
            .get(chapter_index)
            .ok_or(EpubError::Missing("spine item"))?
            .path
            .clone();
        let path = resolve_path(&chapter_path, href);
        let bytes = read_entry_bytes(&mut self.archive, &path)?;
        Ok((path, bytes))
    }
}

fn read_entry(archive: &mut ZipArchive<File>, path: &str) -> Result<String, EpubError> {
    Ok(String::from_utf8(read_entry_bytes(archive, path)?)?)
}

fn read_entry_bytes(archive: &mut ZipArchive<File>, path: &str) -> Result<Vec<u8>, EpubError> {
    let mut entry = archive.by_name(path)?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn parse_rootfile(xml: &str) -> Result<Option<String>, EpubError> {
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Empty(element) | Event::Start(element)
                if local_name(element.name().as_ref()) == "rootfile" =>
            {
                let media_type = attribute(&element, b"media-type")?;
                if media_type.as_deref() == Some("application/oebps-package+xml") {
                    return attribute(&element, b"full-path");
                }
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
        buf.clear();
    }
}

#[derive(Debug)]
struct ParsedOpf {
    metadata: BookMetadata,
    spine: Vec<SpineItem>,
    nav_path: Option<String>,
    ncx_path: Option<String>,
}

fn parse_opf(xml: &str, opf_path: &str) -> Result<ParsedOpf, EpubError> {
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut manifest = HashMap::new();
    let mut nav_path = None;
    let mut ncx_path = None;
    let mut spine_refs = Vec::new();
    let mut title = String::new();
    let mut authors = Vec::new();
    let mut current_text_tag = None::<String>;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(element) => {
                let name = local_name(element.name().as_ref());
                if name == "title" || name == "creator" {
                    current_text_tag = Some(name.clone());
                }
                if name == "item" {
                    record_manifest_item(
                        &element,
                        opf_path,
                        &mut manifest,
                        &mut nav_path,
                        &mut ncx_path,
                    )?;
                }
                if name == "itemref" {
                    let idref = attribute(&element, b"idref")?;
                    let linear = attribute(&element, b"linear")?.is_none_or(|value| value != "no");
                    if let Some(idref) = idref {
                        spine_refs.push((idref, linear));
                    }
                }
            }
            Event::Empty(element) => {
                let name = local_name(element.name().as_ref());
                if name == "item" {
                    record_manifest_item(
                        &element,
                        opf_path,
                        &mut manifest,
                        &mut nav_path,
                        &mut ncx_path,
                    )?;
                }
                if name == "itemref" {
                    let idref = attribute(&element, b"idref")?;
                    let linear = attribute(&element, b"linear")?.is_none_or(|value| value != "no");
                    if let Some(idref) = idref {
                        spine_refs.push((idref, linear));
                    }
                }
            }
            Event::Text(text) => {
                if let Some(tag) = &current_text_tag {
                    let value = text
                        .xml_content(quick_xml::XmlVersion::Implicit1_0)?
                        .trim()
                        .to_owned();
                    if tag == "title" && title.is_empty() {
                        title = value;
                    } else if tag == "creator" && !value.is_empty() {
                        authors.push(value);
                    }
                }
            }
            Event::End(element) => {
                let name = local_name(element.name().as_ref());
                if matches!(name.as_str(), "title" | "creator") {
                    current_text_tag = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    let spine = spine_refs
        .into_iter()
        .filter_map(|(idref, linear)| {
            manifest
                .get(&idref)
                .cloned()
                .map(|path| SpineItem { path, linear })
        })
        .collect::<Vec<_>>();

    Ok(ParsedOpf {
        metadata: BookMetadata {
            title: if title.is_empty() {
                "Untitled".to_owned()
            } else {
                title
            },
            authors,
        },
        spine,
        nav_path,
        ncx_path,
    })
}

fn record_manifest_item(
    element: &quick_xml::events::BytesStart<'_>,
    opf_path: &str,
    manifest: &mut HashMap<String, String>,
    nav_path: &mut Option<String>,
    ncx_path: &mut Option<String>,
) -> Result<(), EpubError> {
    let (Some(id), Some(href)) = (attribute(element, b"id")?, attribute(element, b"href")?) else {
        return Ok(());
    };
    let path = resolve_path(opf_path, &href);
    if attribute_local(element, b"properties")?.is_some_and(|properties| {
        properties
            .split_whitespace()
            .any(|property| property == "nav")
    }) {
        *nav_path = Some(path.clone());
    }
    if attribute(element, b"media-type")?.as_deref() == Some("application/x-dtbncx+xml") {
        *ncx_path = Some(path.clone());
    }
    manifest.insert(id, path);
    Ok(())
}

/// Parse an EPUB 2 NCX document (`navMap` > `navPoint` > `navLabel`/`content`).
fn parse_ncx(xml: &str, ncx_path: &str, spine: &[SpineItem]) -> Result<Vec<TocEntry>, EpubError> {
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_nav_map = false;
    let mut in_label = false;
    let mut label = String::new();
    let mut pending_label = None::<String>;
    let mut entries = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(element) => match local_name(element.name().as_ref()).as_str() {
                "navmap" => in_nav_map = true,
                "navlabel" if in_nav_map => {
                    in_label = true;
                    label.clear();
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_label {
                    label.push_str(text.xml_content(quick_xml::XmlVersion::Implicit1_0)?.trim());
                }
            }
            Event::CData(data) => {
                if in_label {
                    label.push_str(&String::from_utf8_lossy(&data.into_inner()));
                }
            }
            Event::Empty(element) => {
                if in_nav_map && local_name(element.name().as_ref()) == "content" {
                    if let (Some(label), Some(src)) =
                        (pending_label.take(), attribute(&element, b"src")?)
                    {
                        let target =
                            resolve_path(ncx_path, src.split('#').next().unwrap_or_default());
                        if let Some(spine_index) =
                            spine.iter().position(|item| item.path == target)
                        {
                            if !label.is_empty() {
                                entries.push(TocEntry { label, spine_index });
                            }
                        }
                    }
                }
            }
            Event::End(element) => match local_name(element.name().as_ref()).as_str() {
                "navmap" => in_nav_map = false,
                "navlabel" if in_label => {
                    in_label = false;
                    pending_label = Some(normalize_text(&label));
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(entries)
}

fn parse_nav(xhtml: &str, nav_path: &str, spine: &[SpineItem]) -> Result<Vec<TocEntry>, EpubError> {
    let mut reader = Reader::from_reader(Cursor::new(xhtml));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut depth = 0_usize;
    let mut toc_depth = None;
    let mut link = None::<(String, String)>;
    let mut entries = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(element) => {
                depth += 1;
                let name = local_name(element.name().as_ref());
                if name == "nav"
                    && attribute_local(&element, b"type")?
                        .is_some_and(|value| value.split_whitespace().any(|item| item == "toc"))
                {
                    toc_depth = Some(depth);
                } else if name == "a" && toc_depth.is_some() {
                    if let Some(href) = attribute(&element, b"href")? {
                        link = Some((href, String::new()));
                    }
                }
            }
            Event::Text(text) => {
                if let Some((_, label)) = &mut link {
                    label.push_str(text.xml_content(quick_xml::XmlVersion::Implicit1_0)?.trim());
                }
            }
            Event::CData(data) => {
                if let Some((_, label)) = &mut link {
                    label.push_str(&String::from_utf8_lossy(&data.into_inner()));
                }
            }
            Event::End(element) => {
                let name = local_name(element.name().as_ref());
                if name == "a" {
                    if let Some((href, label)) = link.take() {
                        let target_path =
                            resolve_path(nav_path, href.split('#').next().unwrap_or_default());
                        if let Some(spine_index) =
                            spine.iter().position(|item| item.path == target_path)
                        {
                            let label = normalize_text(&label);
                            if !label.is_empty() {
                                entries.push(TocEntry { label, spine_index });
                            }
                        }
                    }
                }
                if name == "nav" && toc_depth == Some(depth) {
                    toc_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(entries)
}

fn parse_chapter(xhtml: &str) -> Vec<SourcedBlock> {
    let mut reader = Reader::from_reader(Cursor::new(xhtml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut stack = Vec::<ElementState>::new();
    let mut blocks = Vec::new();
    let mut sibling_counts = Vec::<HashMap<String, usize>>::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(Event::Start(element)) => {
                let name = local_name(element.name().as_ref());
                let ordinal = sibling_counts.last_mut().map_or(1, |counts| {
                    let count = counts.entry(name.clone()).or_insert(0);
                    *count += 1;
                    *count
                });
                let (alt, href) = if name == "img" {
                    (
                        attribute(&element, b"alt").ok().flatten(),
                        attribute(&element, b"src").ok().flatten(),
                    )
                } else if name == "image" {
                    (None, attribute_local(&element, b"href").ok().flatten())
                } else {
                    (None, None)
                };
                stack.push(ElementState {
                    name,
                    ordinal,
                    text: String::new(),
                    alt,
                    href,
                });
                sibling_counts.push(HashMap::new());
            }
            Ok(Event::Empty(element)) => {
                let name = local_name(element.name().as_ref());
                if name == "img" || name == "image" || name == "svg" {
                    let alt = attribute(&element, b"alt").ok().flatten();
                    let href = if name == "img" {
                        attribute(&element, b"src").ok().flatten()
                    } else {
                        attribute_local(&element, b"href").ok().flatten()
                    };
                    blocks.push(SourcedBlock {
                        block: Block::Image { alt, href },
                        source_path: source_path(&stack, &name, 1),
                    });
                } else if name == "hr" {
                    blocks.push(SourcedBlock {
                        block: Block::Rule,
                        source_path: source_path(&stack, &name, 1),
                    });
                } else if name == "br" {
                    if let Some(parent) = stack.last_mut() {
                        parent.text.push('\n');
                    }
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(element) = stack.last_mut() {
                    if let Ok(value) = text.xml_content(quick_xml::XmlVersion::Implicit1_0) {
                        element.text.push_str(&value);
                    }
                }
            }
            Ok(Event::CData(data)) => {
                if let Some(element) = stack.last_mut() {
                    element
                        .text
                        .push_str(&String::from_utf8_lossy(&data.into_inner()));
                }
            }
            Ok(Event::End(_)) => {
                if let Some(element) = stack.pop() {
                    sibling_counts.pop();
                    // <pre> is the one element where whitespace is meaningful.
                    let text = if element.name == "pre" {
                        normalize_pre_text(&element.text)
                    } else {
                        normalize_text(&element.text)
                    };
                    if let Some(block) =
                        block_for_element(&element.name, text, element.alt, element.href)
                    {
                        blocks.push(SourcedBlock {
                            block,
                            source_path: source_path(&stack, &element.name, element.ordinal),
                        });
                    } else if let Some(parent) = stack.last_mut() {
                        parent.text.push_str(&element.text);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    blocks
}

#[derive(Debug)]
struct ElementState {
    name: String,
    ordinal: usize,
    text: String,
    alt: Option<String>,
    href: Option<String>,
}

fn block_for_element(
    name: &str,
    text: String,
    alt: Option<String>,
    href: Option<String>,
) -> Option<Block> {
    match name {
        "p" | "li" | "td" | "figcaption" if !text.is_empty() => Some(Block::Paragraph(text)),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if !text.is_empty() => {
            let level = name
                .get(1..)
                .and_then(|digit| digit.parse().ok())
                .unwrap_or(1);
            Some(Block::Heading { level, text })
        }
        "blockquote" if !text.is_empty() => Some(Block::Quote(text)),
        "pre" if !text.is_empty() => Some(Block::Code(text)),
        "hr" => Some(Block::Rule),
        // svg is only handled as Event::Empty; a non-empty <svg> defers to its inner <image>
        "img" | "image" => Some(Block::Image { alt, href }),
        _ => None,
    }
}

fn source_path(stack: &[ElementState], name: &str, ordinal: usize) -> Vec<SourcePathStep> {
    stack
        .iter()
        .map(|element| SourcePathStep {
            name: element.name.clone(),
            ordinal: element.ordinal,
        })
        .chain(std::iter::once(SourcePathStep {
            name: name.to_owned(),
            ordinal,
        }))
        .collect()
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keep line structure and indentation, only trimming surrounding blank lines.
fn normalize_pre_text(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn attribute(
    element: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
) -> Result<Option<String>, EpubError> {
    for attribute in element.attributes().with_checks(false) {
        let attribute = attribute.map_err(quick_xml::Error::InvalidAttr)?;
        if attribute.key.as_ref() == key {
            return Ok(Some(
                attribute
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn attribute_local(
    element: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
) -> Result<Option<String>, EpubError> {
    for attribute in element.attributes().with_checks(false) {
        let attribute = attribute.map_err(quick_xml::Error::InvalidAttr)?;
        if local_name(attribute.key.as_ref()).as_bytes() == key {
            return Ok(Some(
                attribute
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn local_name(name: &[u8]) -> String {
    let start = name
        .iter()
        .rposition(|byte| *byte == b':')
        .map_or(0, |index| index + 1);
    String::from_utf8_lossy(name.get(start..).unwrap_or_default()).to_ascii_lowercase()
}

fn resolve_path(opf_path: &str, href: &str) -> String {
    let base = Path::new(opf_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    // Hrefs are URLs (spaces become %20), ZIP entry names are raw.
    let path = base.join(percent_decode(href));
    normalize_archive_path(&path)
}

fn percent_decode(href: &str) -> String {
    let mut out = Vec::with_capacity(href.len());
    let mut rest = href.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            if let Some(value) = tail
                .get(..2)
                .and_then(|hex| u8::from_str_radix(&String::from_utf8_lossy(hex), 16).ok())
            {
                out.push(value);
                rest = tail.get(2..).unwrap_or_default();
                continue;
            }
        }
        out.push(byte);
        rest = tail;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn normalize_archive_path(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => normalized.push(segment),
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => {}
        }
    }
    normalized.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn parses_blocks_and_images() {
        let blocks = parse_chapter(
            "<html><body><h1>Title</h1><p>Hello <em>world</em>.</p><img alt='Map'/></body></html>",
        );
        assert_eq!(blocks.len(), 3);
        assert!(matches!(
            blocks.last().map(|block| &block.block),
            Some(Block::Image { .. })
        ));
    }

    #[test]
    fn svg_cover_emits_single_image() {
        let blocks =
            parse_chapter("<html><body><svg><image href='cover.jpg'/></svg></body></html>");
        let images = blocks
            .iter()
            .filter(|block| matches!(block.block, Block::Image { .. }))
            .count();
        assert_eq!(images, 1);
    }

    #[test]
    fn resolves_relative_opf_paths() {
        assert_eq!(
            resolve_path("OPS/package.opf", "Text/chapter.xhtml"),
            "OPS/Text/chapter.xhtml"
        );
    }

    #[test]
    fn resolve_path_decodes_percent_escapes() {
        assert_eq!(
            resolve_path("OPS/package.opf", "Text/chapter%20one%20%26%20two.xhtml"),
            "OPS/Text/chapter one & two.xhtml"
        );
        // Malformed escapes pass through verbatim.
        assert_eq!(resolve_path("", "Text/100%25.xhtml"), "Text/100%.xhtml");
        assert_eq!(resolve_path("", "Text/50%.xhtml"), "Text/50%.xhtml");
    }

    #[test]
    fn pre_blocks_keep_line_structure() {
        let blocks = parse_chapter(
            "<html><body><pre>\nfn main() {\n    run();\n}\n</pre><p>After\nthe code.</p></body></html>",
        );
        assert_eq!(blocks.len(), 2);
        assert!(matches!(
            &blocks[0].block,
            Block::Code(text) if text == "fn main() {\n    run();\n}"
        ));
        assert!(matches!(
            &blocks[1].block,
            Block::Paragraph(text) if text == "After the code."
        ));
    }

    #[test]
    fn parses_epub3_navigation_titles() -> Result<(), EpubError> {
        let spine = vec![
            SpineItem {
                path: "OPS/Text/one.xhtml".to_owned(),
                linear: true,
            },
            SpineItem {
                path: "OPS/Text/two.xhtml".to_owned(),
                linear: true,
            },
        ];
        let nav = r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body>
            <nav epub:type="toc"><ol>
                <li><a href="Text/one.xhtml">Chapter 1: Dawn</a></li>
                <li><a href="Text/two.xhtml#section">The Time I Fought 30 Snakes</a></li>
            </ol></nav>
        </body></html>"#;

        let toc = parse_nav(nav, "OPS/nav.xhtml", &spine)?;
        assert_eq!(toc.len(), 2);
        assert_eq!(toc[0].label, "Chapter 1: Dawn");
        assert_eq!(toc[1].spine_index, 1);
        assert_eq!(toc[1].label, "The Time I Fought 30 Snakes");
        Ok(())
    }

    #[test]
    fn parses_epub2_ncx_titles() -> Result<(), EpubError> {
        let spine = vec![
            SpineItem {
                path: "OPS/Text/one.xhtml".to_owned(),
                linear: true,
            },
            SpineItem {
                path: "OPS/Text/two.xhtml".to_owned(),
                linear: true,
            },
        ];
        let ncx = r#"<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
            <navMap>
                <navPoint id="n1" playOrder="1">
                    <navLabel><text>First Steps</text></navLabel>
                    <content src="Text/one.xhtml"/>
                    <navPoint id="n2" playOrder="2">
                        <navLabel><text>Nested Finale</text></navLabel>
                        <content src="Text/two.xhtml#part"/>
                    </navPoint>
                </navPoint>
            </navMap>
        </ncx>"#;

        let toc = parse_ncx(ncx, "OPS/toc.ncx", &spine)?;
        assert_eq!(toc.len(), 2);
        assert_eq!(toc[0].label, "First Steps");
        assert_eq!(toc[0].spine_index, 0);
        assert_eq!(toc[1].label, "Nested Finale");
        assert_eq!(toc[1].spine_index, 1);
        Ok(())
    }

    #[test]
    fn image_blocks_capture_href() {
        let blocks = parse_chapter(
            "<html><body><img src='../Images/map.png' alt='Map'/><svg><image xlink:href='cover.jpg'/></svg></body></html>",
        );
        let hrefs: Vec<Option<&str>> = blocks
            .iter()
            .filter_map(|block| match &block.block {
                Block::Image { href, .. } => Some(href.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(
            hrefs,
            vec![Some("../Images/map.png"), Some("cover.jpg")]
        );
    }
}
