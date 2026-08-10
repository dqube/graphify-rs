//! Office document text extraction: `.docx` and `.xlsx` to markdown.
//!
//! Both formats are zip archives of XML parts, so the content is read directly
//! rather than through a document-model library. Output is markdown because
//! that is what the rest of the pipeline already understands: headings survive
//! as headings, tables as pipe tables.
//!
//! A corpus is attacker-controllable — graphify runs on cloned and shared
//! folders — and a few-KB zip bomb can decompress to gigabytes. Every archive
//! is screened before any XML parser touches it, and decompression is bounded
//! by a hard byte ceiling as it happens, because the sizes declared in the zip
//! central directory are themselves attacker-controlled.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use tracing::debug;

/// Largest office file we will open at all.
const MAX_RAW_BYTES: u64 = 50 * 1024 * 1024;
/// Ceiling on total decompressed bytes across all parts we read.
const MAX_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
/// Declared uncompressed : compressed ratio above which we assume a bomb.
const MAX_COMPRESSION_RATIO: u64 = 200;

type Archive = zip::ZipArchive<BufReader<File>>;

/// True when `path` is an office document this module can convert.
pub fn is_office_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("docx") | Some("xlsx")
    )
}

/// Convert an office document to markdown, or `None` if it cannot be read.
///
/// Never returns an error: an unreadable or hostile document should drop out of
/// the corpus, not abort the scan.
pub fn office_to_markdown(path: &Path) -> Option<String> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("docx") => docx_to_markdown(path),
        Some("xlsx") => xlsx_to_markdown(path),
        _ => None,
    }
}

/// Open an archive after screening it for zip-bomb characteristics.
///
/// This is the cheap half of the guard: it rejects an honest bomb using only
/// the central-directory sizes, without decompressing anything. The
/// authoritative half is [`read_member`], which bounds actual decompression.
fn open_checked(path: &Path) -> Option<Archive> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_RAW_BYTES {
        debug!("office: {} exceeds the size cap", path.display());
        return None;
    }

    let mut archive = zip::ZipArchive::new(BufReader::new(File::open(path).ok()?)).ok()?;

    let (mut declared, mut compressed) = (0u64, 0u64);
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            return None;
        };
        declared = declared.saturating_add(entry.size());
        compressed = compressed.saturating_add(entry.compressed_size());
    }
    if declared > MAX_DECOMPRESSED_BYTES {
        debug!("office: {} declares too much content", path.display());
        return None;
    }
    if declared / compressed.max(1) > MAX_COMPRESSION_RATIO {
        debug!("office: {} has a bomb-like ratio", path.display());
        return None;
    }

    Some(archive)
}

/// Read one archive member as text, drawing from a shared decompression budget.
///
/// Returns `None` if the member is missing or would exhaust the budget — a
/// member that under-declares its size in the central directory cannot expand
/// past the ceiling undetected, because the limit is applied to the bytes
/// actually produced.
fn read_member(archive: &mut Archive, name: &str, budget: &mut u64) -> Option<String> {
    let entry = archive.by_name(name).ok()?;
    let limit = *budget;
    let mut buf = Vec::new();
    entry
        .take(limit.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > limit {
        debug!("office: member {name} exceeded the decompression budget");
        return None;
    }
    *budget -= buf.len() as u64;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Tag name without its namespace prefix (`w:p` -> `p`).
///
/// Takes the `QName` rather than the event, so it serves both start and end
/// tags — they are distinct types that share this accessor.
fn local(name: QName<'_>) -> String {
    String::from_utf8_lossy(name.local_name().as_ref()).into_owned()
}

/// Value of an attribute, matched by local name (`w:val` -> `val`).
fn attr(e: &BytesStart<'_>, want: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.local_name().as_ref() == want.as_bytes())
            .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
            .flatten()
    })
}

// ── .docx ────────────────────────────────────────────────────────────────────

fn docx_to_markdown(path: &Path) -> Option<String> {
    let mut archive = open_checked(path)?;
    let mut budget = MAX_DECOMPRESSED_BYTES;
    let xml = read_member(&mut archive, "word/document.xml", &mut budget)?;
    Some(parse_docx(&xml))
}

/// Map a Word style to its markdown prefix.
///
/// The XML carries style *ids* (`Heading1`), while the same style is named
/// `Heading 1` in the UI, so spaces are normalised away before matching.
fn style_prefix(style: &str) -> &'static str {
    let norm = style.replace(' ', "").to_ascii_lowercase();
    if norm.starts_with("heading1") {
        "# "
    } else if norm.starts_with("heading2") {
        "## "
    } else if norm.starts_with("heading3") {
        "### "
    } else if norm.starts_with("list") {
        "- "
    } else {
        ""
    }
}

fn parse_docx(xml: &str) -> String {
    let mut reader = Reader::from_str(xml);
    let mut lines: Vec<String> = Vec::new();
    let mut tables: Vec<Vec<Vec<String>>> = Vec::new();

    let mut para = String::new();
    let mut style: Option<String> = None;
    let mut cell = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut table_depth = 0usize;
    let mut in_text = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local(e.name()).as_str() {
                "tbl" => {
                    table_depth += 1;
                    if table_depth == 1 {
                        table.clear();
                    }
                }
                "tr" if table_depth == 1 => row.clear(),
                "tc" if table_depth == 1 => cell.clear(),
                "t" => in_text = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name()).as_str() {
                "pStyle" if table_depth == 0 => style = attr(&e, "val"),
                "br" | "tab" => {
                    if table_depth == 0 {
                        para.push(' ');
                    } else {
                        cell.push(' ');
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                let text = t.unescape().unwrap_or_default();
                if table_depth == 0 {
                    para.push_str(&text);
                } else {
                    cell.push_str(&text);
                }
            }
            Ok(Event::End(e)) => match local(e.name()).as_str() {
                "t" => in_text = false,
                "p" if table_depth == 0 => {
                    let text = para.trim();
                    if text.is_empty() {
                        lines.push(String::new());
                    } else {
                        lines.push(format!(
                            "{}{text}",
                            style_prefix(style.as_deref().unwrap_or(""))
                        ));
                    }
                    para.clear();
                    style = None;
                }
                "tc" if table_depth == 1 => {
                    row.push(cell.trim().to_string());
                    cell.clear();
                }
                "tr" if table_depth == 1 => {
                    table.push(std::mem::take(&mut row));
                }
                "tbl" => {
                    table_depth = table_depth.saturating_sub(1);
                    if table_depth == 0 && !table.is_empty() {
                        tables.push(std::mem::take(&mut table));
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => {
                debug!("office: malformed docx XML: {e}");
                break;
            }
            _ => {}
        }
    }

    for t in &tables {
        lines.extend(rows_to_markdown(t));
    }

    lines.join("\n")
}

// ── .xlsx ────────────────────────────────────────────────────────────────────

fn xlsx_to_markdown(path: &Path) -> Option<String> {
    let mut archive = open_checked(path)?;
    let mut budget = MAX_DECOMPRESSED_BYTES;

    let shared = read_member(&mut archive, "xl/sharedStrings.xml", &mut budget)
        .map(|s| parse_shared_strings(&s))
        .unwrap_or_default();
    let workbook = read_member(&mut archive, "xl/workbook.xml", &mut budget)?;
    let rels = read_member(&mut archive, "xl/_rels/workbook.xml.rels", &mut budget)
        .map(|s| parse_rels(&s))
        .unwrap_or_default();

    let mut out: Vec<String> = Vec::new();
    for (index, (name, rel_id)) in parse_sheet_list(&workbook).into_iter().enumerate() {
        // Sheet order in workbook.xml is authoritative; the relationship maps it
        // to a part name, which need not be sheet1.xml, sheet2.xml, … in order.
        let member = rel_id
            .and_then(|id| rels.get(&id).cloned())
            .map(|target| normalise_target(&target))
            .unwrap_or_else(|| format!("xl/worksheets/sheet{}.xml", index + 1));

        let Some(xml) = read_member(&mut archive, &member, &mut budget) else {
            continue;
        };
        let rows = parse_sheet(&xml, &shared);
        if rows.is_empty() {
            continue;
        }
        out.push(format!("## Sheet: {name}"));
        out.extend(rows_to_markdown(&rows));
    }

    Some(out.join("\n"))
}

/// A relationship target is relative to `xl/`, but may be given absolutely.
fn normalise_target(target: &str) -> String {
    let t = target.trim_start_matches('/');
    if t.starts_with("xl/") {
        t.to_string()
    } else {
        format!("xl/{t}")
    }
}

/// Shared strings, indexed by position. Each `<si>` may hold several runs.
fn parse_shared_strings(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_item = false;
    let mut in_text = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local(e.name()).as_str() {
                "si" => {
                    in_item = true;
                    current.clear();
                }
                "t" if in_item => in_text = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                current.push_str(&t.unescape().unwrap_or_default());
            }
            Ok(Event::End(e)) => match local(e.name()).as_str() {
                "t" => in_text = false,
                "si" => {
                    out.push(std::mem::take(&mut current));
                    in_item = false;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// `rId1` -> `worksheets/sheet1.xml`
fn parse_rels(xml: &str) -> HashMap<String, String> {
    let mut reader = Reader::from_str(xml);
    let mut out = HashMap::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if local(e.name()) == "Relationship"
                    && let (Some(id), Some(target)) = (attr(&e, "Id"), attr(&e, "Target"))
                {
                    out.insert(id, target);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Sheet names in workbook order, each with its relationship id.
fn parse_sheet_list(xml: &str) -> Vec<(String, Option<String>)> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if local(e.name()) == "sheet"
                    && let Some(name) = attr(&e, "name")
                {
                    out.push((name, attr(&e, "id")));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Zero-based column index from a cell reference (`B2` -> 1).
fn column_index(cell_ref: &str) -> usize {
    let mut index = 0usize;
    for c in cell_ref.chars() {
        if !c.is_ascii_alphabetic() {
            break;
        }
        index = index * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1);
    }
    index.saturating_sub(1)
}

/// Rows of a worksheet, with cells placed at their true column positions.
fn parse_sheet(xml: &str, shared: &[String]) -> Vec<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut value = String::new();
    let mut cell_type: Option<String> = None;
    let mut column = 0usize;
    let mut in_value = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local(e.name()).as_str() {
                "row" => row.clear(),
                "c" => {
                    column = attr(&e, "r").map_or(row.len(), |r| column_index(&r));
                    cell_type = attr(&e, "t");
                    value.clear();
                }
                // `v` holds a literal or a shared-string index; `t` inside `is`
                // holds inline text.
                "v" | "t" => in_value = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_value => {
                value.push_str(&t.unescape().unwrap_or_default());
            }
            Ok(Event::End(e)) => match local(e.name()).as_str() {
                "v" | "t" => in_value = false,
                "c" => {
                    let text = match cell_type.as_deref() {
                        Some("s") => value
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| shared.get(i).cloned())
                            .unwrap_or_default(),
                        Some("b") => {
                            if value.trim() == "1" {
                                "True".to_string()
                            } else {
                                "False".to_string()
                            }
                        }
                        _ => value.clone(),
                    };
                    if row.len() <= column {
                        row.resize(column + 1, String::new());
                    }
                    row[column] = text;
                    value.clear();
                    cell_type = None;
                }
                "row" => rows.push(std::mem::take(&mut row)),
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    // An entirely empty row carries nothing; dropping it also keeps a stray
    // trailing row from being chosen as the header.
    rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    rows
}

// ── shared table rendering ───────────────────────────────────────────────────

/// Render rows as a GitHub pipe table, first row as the header.
fn rows_to_markdown(rows: &[Vec<String>]) -> Vec<String> {
    let Some(width) = rows.iter().map(Vec::len).max().filter(|w| *w > 0) else {
        return Vec::new();
    };

    let render = |row: &Vec<String>| {
        let mut cells: Vec<String> = row.iter().map(|c| escape_cell(c)).collect();
        cells.resize(width, String::new());
        format!("| {} |", cells.join(" | "))
    };

    let mut out = vec![
        render(&rows[0]),
        format!("| {} |", vec!["---"; width].join(" | ")),
    ];
    out.extend(rows[1..].iter().map(render));
    out
}

/// A literal `|` would break the surrounding pipe table.
fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn docx_xml(body: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{body}</w:body></w:document>"#
        )
    }

    fn para(style: Option<&str>, text: &str) -> String {
        let style = style.map_or(String::new(), |s| {
            format!(r#"<w:pPr><w:pStyle w:val="{s}"/></w:pPr>"#)
        });
        format!("<w:p>{style}<w:r><w:t>{text}</w:t></w:r></w:p>")
    }

    /// Build a zip in memory so tests exercise the real archive path.
    fn write_zip(dir: &Path, name: &str, members: &[(&str, &str)]) -> std::path::PathBuf {
        let path = dir.join(name);
        let file = File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (member, body) in members {
            zw.start_file(*member, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    #[test]
    fn recognises_office_paths() {
        assert!(is_office_path(Path::new("a/report.docx")));
        assert!(is_office_path(Path::new("a/Budget.XLSX")));
        assert!(!is_office_path(Path::new("a/notes.md")));
        assert!(!is_office_path(Path::new("a/deck.pptx")));
    }

    #[test]
    fn docx_headings_and_lists_become_markdown() {
        let xml = docx_xml(&format!(
            "{}{}{}{}{}",
            para(Some("Heading1"), "Title"),
            para(Some("Heading 2"), "Section"),
            para(Some("Heading3"), "Detail"),
            para(Some("ListParagraph"), "a bullet"),
            para(None, "Body text.")
        ));
        let md = parse_docx(&xml);
        assert!(md.contains("# Title"));
        // The style *name* form ("Heading 2") must work as well as the id form.
        assert!(md.contains("## Section"));
        assert!(md.contains("### Detail"));
        assert!(md.contains("- a bullet"));
        assert!(md.contains("Body text."));
    }

    #[test]
    fn docx_runs_are_joined_and_entities_decoded() {
        let xml =
            docx_xml("<w:p><w:r><w:t>Tom </w:t></w:r><w:r><w:t>&amp; Jerry</w:t></w:r></w:p>");
        assert!(parse_docx(&xml).contains("Tom & Jerry"));
    }

    #[test]
    fn docx_tables_render_as_pipe_tables() {
        let xml = docx_xml(
            "<w:tbl>\
               <w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc>\
                     <w:tc><w:p><w:r><w:t>Qty</w:t></w:r></w:p></w:tc></w:tr>\
               <w:tr><w:tc><w:p><w:r><w:t>Bolt</w:t></w:r></w:p></w:tc>\
                     <w:tc><w:p><w:r><w:t>12</w:t></w:r></w:p></w:tc></w:tr>\
             </w:tbl>",
        );
        let md = parse_docx(&xml);
        assert!(md.contains("| Name | Qty |"));
        assert!(md.contains("| --- | --- |"));
        assert!(md.contains("| Bolt | 12 |"));
    }

    #[test]
    fn docx_table_text_does_not_leak_into_paragraphs() {
        let xml = docx_xml(&format!(
            "{}<w:tbl><w:tr><w:tc><w:p><w:r><w:t>InCell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>",
            para(None, "Intro")
        ));
        let md = parse_docx(&xml);
        let before_table = md.split("| InCell |").next().unwrap();
        assert!(before_table.contains("Intro"));
        // "InCell" must appear only inside the rendered table row.
        assert_eq!(md.matches("InCell").count(), 1);
    }

    #[test]
    fn xlsx_sheets_become_tables_with_shared_strings() {
        let td = TempDir::new().unwrap();
        let path = write_zip(
            td.path(),
            "book.xlsx",
            &[
                (
                    "xl/workbook.xml",
                    r#"<workbook xmlns:r="x"><sheets><sheet name="Inventory" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
                ),
                (
                    "xl/sharedStrings.xml",
                    r#"<sst><si><t>Item</t></si><si><t>Count</t></si><si><t>Bolt</t></si></sst>"#,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    r#"<worksheet><sheetData>
                       <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
                       <row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>12</v></c></row>
                       </sheetData></worksheet>"#,
                ),
            ],
        );

        let md = office_to_markdown(&path).expect("expected markdown");
        assert!(md.contains("## Sheet: Inventory"));
        assert!(md.contains("| Item | Count |"));
        assert!(md.contains("| Bolt | 12 |"));
    }

    #[test]
    fn xlsx_gaps_keep_cells_in_their_columns() {
        let td = TempDir::new().unwrap();
        let path = write_zip(
            td.path(),
            "gaps.xlsx",
            &[
                (
                    "xl/workbook.xml",
                    r#"<workbook><sheets><sheet name="S" sheetId="1"/></sheets></workbook>"#,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    r#"<worksheet><sheetData>
                       <row r="1"><c r="A1"><v>1</v></c><c r="C1"><v>3</v></c></row>
                       </sheetData></worksheet>"#,
                ),
            ],
        );
        // A skipped B column must stay empty rather than shifting C left.
        assert!(office_to_markdown(&path).unwrap().contains("| 1 |  | 3 |"));
    }

    #[test]
    fn xlsx_blank_rows_are_dropped() {
        let td = TempDir::new().unwrap();
        let path = write_zip(
            td.path(),
            "blank.xlsx",
            &[
                (
                    "xl/workbook.xml",
                    r#"<workbook><sheets><sheet name="S" sheetId="1"/></sheets></workbook>"#,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    r#"<worksheet><sheetData>
                       <row r="1"><c r="A1" t="inlineStr"><is><t>Header</t></is></c></row>
                       <row r="2"><c r="A2"/></row>
                       <row r="3"><c r="A3" t="inlineStr"><is><t>Value</t></is></c></row>
                       </sheetData></worksheet>"#,
                ),
            ],
        );
        let md = office_to_markdown(&path).unwrap();
        assert!(md.contains("| Header |"));
        assert!(md.contains("| Value |"));
        assert_eq!(md.lines().filter(|l| l.starts_with("| ")).count(), 3);
    }

    #[test]
    fn pipes_in_cells_do_not_break_the_table() {
        let rows = vec![vec!["a|b".to_string()], vec!["c".to_string()]];
        assert!(rows_to_markdown(&rows)[0].contains("a\\|b"));
    }

    #[test]
    fn column_references_decode_past_z() {
        assert_eq!(column_index("A1"), 0);
        assert_eq!(column_index("B2"), 1);
        assert_eq!(column_index("Z9"), 25);
        assert_eq!(column_index("AA1"), 26);
        assert_eq!(column_index("AB1"), 27);
    }

    #[test]
    fn a_non_zip_file_is_rejected_rather_than_parsed() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("fake.docx");
        std::fs::write(&path, b"this is not a zip archive").unwrap();
        assert!(office_to_markdown(&path).is_none());
    }

    #[test]
    fn a_highly_compressible_member_is_rejected_as_a_bomb() {
        let td = TempDir::new().unwrap();
        // 8 MiB of zeros compresses far past the ratio cap.
        let bomb = "0".repeat(8 * 1024 * 1024);
        let path = write_zip(td.path(), "bomb.docx", &[("word/document.xml", &bomb)]);
        assert!(
            office_to_markdown(&path).is_none(),
            "a bomb-ratio archive must be refused"
        );
    }

    #[test]
    fn missing_document_part_yields_nothing() {
        let td = TempDir::new().unwrap();
        let path = write_zip(td.path(), "empty.docx", &[("docProps/app.xml", "<x/>")]);
        assert!(office_to_markdown(&path).is_none());
    }
}
