//! Pages whose text sits mostly in an invisible layer over a scan.
//!
//! A scanned page made searchable carries its words as invisible text
//! (render mode 3, or 7, which only adds to the clip) behind the page image.
//! When the page also shows a little real text, such as a header or a Bates
//! number, pdf-inspector 1.24.0 classifies it as text, extracts the visible
//! text alone, and reports no page for OCR: it flags only a page whose every
//! text operator is invisible. Upstream pull requests #479 and #501 are open
//! against this.
//!
//! This scan finds the page it misses: images cover at least half of it, and
//! invisible text carries most of the bytes it shows as text. The layer is
//! never read into any output, since what it says need not be what the page
//! shows; the page is reported as needing OCR instead.

use std::collections::HashSet;

use lopdf::{content::Content, Dictionary, Document, Object, ObjectId, Stream};

/// Bytes any one content stream may decode to.
const MAX_STREAM_BYTES: usize = 32 << 20;
/// Decoded content bytes and operations scanned per document. Past either,
/// the scan stops and reports the pages it has already found.
const MAX_CONTENT_BYTES: usize = 128 << 20;
const MAX_OPERATIONS: usize = 10_000_000;
/// Form XObjects executing inside one another.
const MAX_FORM_DEPTH: usize = 12;
/// Graphics states saved (`q`) and not yet restored, per content stream.
const MAX_SAVED_STATES: usize = 256;
/// Bytes an object stream may decode to while the document loads, as
/// pdf-inspector bounds them.
const MAX_OBJECT_STREAM_BYTES: usize = 8 << 20;
/// Invisible text a page must carry to count as a layer, in string bytes.
const MIN_LAYER_BYTES: u64 = 32;
/// Share of the page that images must cover.
const COVERED_SHARE: f64 = 0.5;
/// Parent links followed to find an inherited page attribute.
const MAX_PAGE_TREE_DEPTH: usize = 32;

const IDENTITY: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// The scan's work limits ran out.
struct Exhausted;

struct Budget {
    bytes: usize,
    operations: usize,
}

impl Budget {
    fn take_bytes(&mut self, bytes: usize) -> Result<(), Exhausted> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > MAX_CONTENT_BYTES {
            return Err(Exhausted);
        }
        Ok(())
    }

    fn take_operations(&mut self, operations: usize) -> Result<(), Exhausted> {
        self.operations = self.operations.saturating_add(operations);
        if self.operations > MAX_OPERATIONS {
            return Err(Exhausted);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct State {
    ctm: [f64; 6],
    render_mode: i64,
}

/// What one page's content shows.
#[derive(Default)]
struct PageText {
    hidden_bytes: u64,
    visible_bytes: u64,
    /// Area of the page the drawn images cover, clipped to the page box,
    /// counting overlaps more than once.
    image_area: f64,
}

impl PageText {
    fn show(&mut self, state: State, text: Option<&Object>) {
        let bytes = match text {
            Some(Object::String(bytes, _)) => bytes.len(),
            Some(Object::Array(parts)) => parts
                .iter()
                .map(|part| match part {
                    Object::String(bytes, _) => bytes.len(),
                    _ => 0,
                })
                .sum(),
            _ => 0,
        } as u64;
        // Mode 3 paints nothing; mode 7 only adds the glyphs to the clip.
        if matches!(state.render_mode, 3 | 7) {
            self.hidden_bytes += bytes;
        } else {
            self.visible_bytes += bytes;
        }
    }

    fn draw_image(&mut self, ctm: [f64; 6], page_box: [f64; 4]) {
        // An image fills the unit square of its user space.
        let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].map(|(x, y)| {
            (
                ctm[0] * x + ctm[2] * y + ctm[4],
                ctm[1] * x + ctm[3] * y + ctm[5],
            )
        });
        let (mut left, mut right) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut bottom, mut top) = (f64::INFINITY, f64::NEG_INFINITY);
        for (x, y) in corners {
            left = left.min(x);
            right = right.max(x);
            bottom = bottom.min(y);
            top = top.max(y);
        }
        let width = (right.min(page_box[2]) - left.max(page_box[0])).max(0.0);
        let height = (top.min(page_box[3]) - bottom.max(page_box[1])).max(0.0);
        let area = width * height;
        if area.is_finite() {
            self.image_area += area;
        }
    }

    fn is_hidden_layer(&self, page_box: [f64; 4]) -> bool {
        let page_area = (page_box[2] - page_box[0]) * (page_box[3] - page_box[1]);
        page_area > 0.0
            && self.image_area >= COVERED_SHARE * page_area
            && self.hidden_bytes >= MIN_LAYER_BYTES
            && self.hidden_bytes > self.visible_bytes
    }
}

/// The 1-indexed pages whose text is mostly an invisible layer over images
/// covering the page. Pages in `skip` are not scanned, nor pages outside
/// `only` when it is given. A document that does not load reports none.
pub(crate) fn pages_with_hidden_text_layer(
    buffer: &[u8],
    skip: &HashSet<u32>,
    only: Option<&HashSet<u32>>,
) -> Vec<u32> {
    let options = lopdf::LoadOptions {
        max_decompressed_size: Some(MAX_OBJECT_STREAM_BYTES),
        ..Default::default()
    };
    let Ok(document) = Document::load_mem_with_options(buffer, options) else {
        return Vec::new();
    };
    let mut budget = Budget {
        bytes: 0,
        operations: 0,
    };
    let mut found = Vec::new();
    for (&number, &page_id) in &document.get_pages() {
        if skip.contains(&number) || only.is_some_and(|only| !only.contains(&number)) {
            continue;
        }
        match scan_page(&document, page_id, &mut budget) {
            Ok(true) => found.push(number),
            Ok(false) => {}
            Err(Exhausted) => break,
        }
    }
    found
}

fn scan_page(
    document: &Document,
    page_id: ObjectId,
    budget: &mut Budget,
) -> Result<bool, Exhausted> {
    let Some(page_box) = page_box(document, page_id) else {
        return Ok(false);
    };
    let resources = page_resources(document, page_id);
    // A scan is an image XObject; a page that binds none, directly or
    // through its forms, is not read.
    let mut seen = HashSet::new();
    if !resources
        .iter()
        .any(|dictionary| binds_image(document, dictionary, 0, &mut seen))
    {
        return Ok(false);
    }
    let mut content = Vec::new();
    for id in document.get_page_contents(page_id) {
        let Ok(stream) = document.get_object(id).and_then(Object::as_stream) else {
            continue;
        };
        let Ok(bytes) = stream.decompressed_content_with_limit(MAX_STREAM_BYTES) else {
            continue;
        };
        budget.take_bytes(bytes.len())?;
        content.extend_from_slice(&bytes);
        // Streams are concatenated as if one, separated by white space.
        content.push(b'\n');
    }
    let mut page = PageText::default();
    let start = State {
        ctm: IDENTITY,
        render_mode: 0,
    };
    let mut forms = Vec::new();
    execute(
        document, &content, &resources, start, page_box, &mut page, &mut forms, budget,
    )?;
    Ok(page.is_hidden_layer(page_box))
}

#[allow(clippy::too_many_arguments)]
fn execute<'a>(
    document: &'a Document,
    content: &[u8],
    resources: &[&'a Dictionary],
    start: State,
    page_box: [f64; 4],
    page: &mut PageText,
    forms: &mut Vec<ObjectId>,
    budget: &mut Budget,
) -> Result<(), Exhausted> {
    let Ok(content) = Content::decode(content) else {
        return Ok(());
    };
    budget.take_operations(content.operations.len())?;
    let mut state = start;
    let mut saved: Vec<State> = Vec::new();
    // Saves past the cap, so their restores are matched too.
    let mut unsaved = 0usize;
    for operation in &content.operations {
        let operands = &operation.operands;
        match operation.operator.as_str() {
            "q" => {
                if saved.len() < MAX_SAVED_STATES {
                    saved.push(state);
                } else {
                    unsaved += 1;
                }
            }
            "Q" => {
                if unsaved > 0 {
                    unsaved -= 1;
                } else if let Some(previous) = saved.pop() {
                    state = previous;
                }
            }
            "cm" => {
                if let Some(matrix) = matrix(document, operands) {
                    state.ctm = multiply(matrix, state.ctm);
                }
            }
            "Tr" => {
                if let Some(mode) = operands.first().and_then(|mode| integer(document, mode)) {
                    state.render_mode = mode;
                }
            }
            "Tj" | "'" | "\"" => page.show(state, operands.last()),
            "TJ" => page.show(state, operands.first()),
            "BI" => page.draw_image(state.ctm, page_box),
            "Do" => {
                let Some(name) = operands.first().and_then(|name| name.as_name().ok()) else {
                    continue;
                };
                let Some((id, stream)) = xobject(document, resources, name) else {
                    continue;
                };
                match stream.dict.get(b"Subtype").and_then(Object::as_name) {
                    Ok(b"Image") => page.draw_image(state.ctm, page_box),
                    Ok(b"Form") => {
                        if forms.len() >= MAX_FORM_DEPTH || forms.contains(&id) {
                            continue;
                        }
                        let Ok(bytes) = stream.decompressed_content_with_limit(MAX_STREAM_BYTES)
                        else {
                            continue;
                        };
                        budget.take_bytes(bytes.len())?;
                        let form_matrix = stream
                            .dict
                            .get(b"Matrix")
                            .ok()
                            .and_then(|matrix| array(document, matrix))
                            .and_then(|values| matrix(document, values))
                            .unwrap_or(IDENTITY);
                        // A form without resources uses its invoker's.
                        let form_resources: Vec<&Dictionary> = match stream
                            .dict
                            .get(b"Resources")
                            .ok()
                            .and_then(|resources| dictionary(document, resources))
                        {
                            Some(own) => vec![own],
                            None => resources.to_vec(),
                        };
                        let inner = State {
                            ctm: multiply(form_matrix, state.ctm),
                            render_mode: state.render_mode,
                        };
                        forms.push(id);
                        let result = execute(
                            document,
                            &bytes,
                            &form_resources,
                            inner,
                            page_box,
                            page,
                            forms,
                            budget,
                        );
                        forms.pop();
                        result?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The product of two PDF matrices `[a b c d e f]`, `first` applied first.
fn multiply(first: [f64; 6], then: [f64; 6]) -> [f64; 6] {
    [
        first[0] * then[0] + first[1] * then[2],
        first[0] * then[1] + first[1] * then[3],
        first[2] * then[0] + first[3] * then[2],
        first[2] * then[1] + first[3] * then[3],
        first[4] * then[0] + first[5] * then[2] + then[4],
        first[4] * then[1] + first[5] * then[3] + then[5],
    ]
}

fn number(document: &Document, object: &Object) -> Option<f64> {
    let (_, object) = document.dereference(object).ok()?;
    let value = match object {
        Object::Integer(value) => *value as f64,
        Object::Real(value) => f64::from(*value),
        _ => return None,
    };
    value.is_finite().then_some(value)
}

fn integer(document: &Document, object: &Object) -> Option<i64> {
    match document.dereference(object).ok()?.1 {
        Object::Integer(value) => Some(*value),
        _ => None,
    }
}

fn matrix(document: &Document, values: &[Object]) -> Option<[f64; 6]> {
    if values.len() != 6 {
        return None;
    }
    let mut matrix = [0.0; 6];
    for (slot, value) in matrix.iter_mut().zip(values) {
        *slot = number(document, value)?;
    }
    Some(matrix)
}

fn array<'a>(document: &'a Document, object: &'a Object) -> Option<&'a [Object]> {
    document
        .dereference(object)
        .ok()?
        .1
        .as_array()
        .ok()
        .map(Vec::as_slice)
}

fn dictionary<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    document.dereference(object).ok()?.1.as_dict().ok()
}

/// The page's resource dictionaries in lookup order: its own, then those it
/// inherits.
fn page_resources(document: &Document, page_id: ObjectId) -> Vec<&Dictionary> {
    let Ok((own, inherited)) = document.get_page_resources(page_id) else {
        return Vec::new();
    };
    own.into_iter()
        .chain(
            inherited
                .into_iter()
                .filter_map(|id| document.get_dictionary(id).ok()),
        )
        .collect()
}

/// The named XObject from the first resource dictionary that binds it, with
/// its object id.
fn xobject<'a>(
    document: &'a Document,
    resources: &[&'a Dictionary],
    name: &[u8],
) -> Option<(ObjectId, &'a Stream)> {
    for resources in resources {
        let Some(xobjects) = resources
            .get(b"XObject")
            .ok()
            .and_then(|xobjects| dictionary(document, xobjects))
        else {
            continue;
        };
        let Ok(Object::Reference(id)) = xobjects.get(name) else {
            continue;
        };
        return document
            .get_object(*id)
            .and_then(Object::as_stream)
            .ok()
            .map(|stream| (*id, stream));
    }
    None
}

/// Whether a resource dictionary binds an image XObject, directly or in the
/// resources of a form it binds.
fn binds_image(
    document: &Document,
    resources: &Dictionary,
    depth: usize,
    seen: &mut HashSet<ObjectId>,
) -> bool {
    let Some(xobjects) = resources
        .get(b"XObject")
        .ok()
        .and_then(|xobjects| dictionary(document, xobjects))
    else {
        return false;
    };
    for (_, entry) in xobjects.iter() {
        let Object::Reference(id) = entry else {
            continue;
        };
        if !seen.insert(*id) {
            continue;
        }
        let Ok(stream) = document.get_object(*id).and_then(Object::as_stream) else {
            continue;
        };
        match stream.dict.get(b"Subtype").and_then(Object::as_name) {
            Ok(b"Image") => return true,
            Ok(b"Form") if depth < MAX_FORM_DEPTH => {
                let nested = stream
                    .dict
                    .get(b"Resources")
                    .ok()
                    .and_then(|resources| dictionary(document, resources));
                if nested.is_some_and(|nested| binds_image(document, nested, depth + 1, seen)) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// The page's visible box in default user space: the crop box within the
/// media box, as `[left, bottom, right, top]`.
fn page_box(document: &Document, page_id: ObjectId) -> Option<[f64; 4]> {
    let media = inherited(document, page_id, b"MediaBox")
        .and_then(|value| rectangle(document, value))
        .unwrap_or([0.0, 0.0, 612.0, 792.0]);
    let visible = match inherited(document, page_id, b"CropBox")
        .and_then(|value| rectangle(document, value))
    {
        Some(crop) => [
            media[0].max(crop[0]),
            media[1].max(crop[1]),
            media[2].min(crop[2]),
            media[3].min(crop[3]),
        ],
        None => media,
    };
    (visible[2] > visible[0] && visible[3] > visible[1]).then_some(visible)
}

fn inherited<'a>(document: &'a Document, page_id: ObjectId, key: &[u8]) -> Option<&'a Object> {
    let mut node = document.get_dictionary(page_id).ok()?;
    for _ in 0..MAX_PAGE_TREE_DEPTH {
        if let Ok(value) = node.get(key) {
            return Some(value);
        }
        let parent = node.get(b"Parent").ok()?.as_reference().ok()?;
        node = document.get_dictionary(parent).ok()?;
    }
    None
}

fn rectangle(document: &Document, value: &Object) -> Option<[f64; 4]> {
    let values = array(document, value)?;
    if values.len() != 4 {
        return None;
    }
    let mut corners = [0.0; 4];
    for (slot, value) in corners.iter_mut().zip(values) {
        *slot = number(document, value)?;
    }
    Some([
        corners[0].min(corners[2]),
        corners[1].min(corners[3]),
        corners[0].max(corners[2]),
        corners[1].max(corners[3]),
    ])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page PDF drawing `page` as its content, with Helvetica as
    /// `/F1`, a 64-pixel gray image as `/Im1`, and a form holding `form` as
    /// `/Fm1`.
    pub(crate) fn scan_pdf(page: &str, form: &str) -> Vec<u8> {
        let pixels = vec![200u8; 64 * 64];
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 6 0 R /Fm1 7 0 R >> >> \
              /Contents 5 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
            stream("", page.as_bytes()),
            stream(
                "/Type /XObject /Subtype /Image /Width 64 /Height 64 \
                 /ColorSpace /DeviceGray /BitsPerComponent 8",
                &pixels,
            ),
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 612 792] \
                 /Resources << /Font << /F1 4 0 R >> >>",
                form.as_bytes(),
            ),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn stream(dictionary: &str, content: &[u8]) -> Vec<u8> {
        let mut object =
            format!("<< {dictionary} /Length {} >>\nstream\n", content.len()).into_bytes();
        object.extend_from_slice(content);
        object.extend_from_slice(b"\nendstream");
        object
    }

    /// Lines of a scanned return, shown in render mode `mode`.
    pub(crate) fn text_layer(mode: u8) -> String {
        let lines = [
            "Form 1040 Individual Income Tax Return",
            "Wages, salaries, tips 85000",
            "Taxable interest 1250",
            "Total income 86250",
        ];
        let shown: String = lines
            .iter()
            .enumerate()
            .map(|(index, line)| format!("1 0 0 1 72 {} Tm ({line}) Tj\n", 700 - 20 * index))
            .collect();
        format!("BT {mode} Tr /F1 11 Tf\n{shown}ET")
    }

    pub(crate) const SCAN: &str = "q 612 0 0 792 0 0 cm /Im1 Do Q";
    pub(crate) const STAMP: &str =
        "BT /F1 9 Tf 1 0 0 1 72 770 Tm (CONFIDENTIAL - CLIENT COPY) Tj ET \
         BT /F1 9 Tf 1 0 0 1 480 20 Tm (BATES-000123) Tj ET";

    fn flagged(page: &str, form: &str) -> bool {
        pages_with_hidden_text_layer(&scan_pdf(page, form), &HashSet::new(), None) == [1]
    }

    #[test]
    fn scans_with_an_invisible_layer_and_visible_chrome_are_found() {
        // The layer in a form, as ocrmypdf writes it, or in the page.
        assert!(flagged(
            &format!("{SCAN} q /Fm1 Do Q {STAMP}"),
            &text_layer(3)
        ));
        assert!(flagged(&format!("{SCAN} {} {STAMP}", text_layer(3)), ""));
        // Clip-only text paints nothing either.
        assert!(flagged(&format!("{SCAN} {} {STAMP}", text_layer(7)), ""));
        // The mode is inherited by a form and restored with the state.
        assert!(flagged(
            &format!("{SCAN} q BT 3 Tr ET /Fm1 Do Q {STAMP}"),
            &text_layer(3).replace("3 Tr", "")
        ));
        assert!(!flagged(
            &format!(
                "{SCAN} q BT 3 Tr ET Q {} {STAMP}",
                text_layer(0).replace("0 Tr", "")
            ),
            ""
        ));
    }

    #[test]
    fn pages_that_show_their_text_are_not_found() {
        // Visible text over a full-page image, as a letterhead or form.
        assert!(!flagged(&format!("{SCAN} {}", text_layer(0)), ""));
        // Invisible text without an image covering the page.
        assert!(!flagged(&text_layer(3), ""));
        assert!(!flagged(
            &format!("q 100 0 0 50 0 0 cm /Im1 Do Q {}", text_layer(3)),
            ""
        ));
        // More visible text than invisible.
        let visible = format!("{} {}", text_layer(0), text_layer(0));
        assert!(!flagged(&format!("{SCAN} {} {visible}", text_layer(3)), ""));
        // A page skipped or outside the pages asked for is not scanned.
        let pdf = scan_pdf(&format!("{SCAN} {} {STAMP}", text_layer(3)), "");
        assert!(pages_with_hidden_text_layer(&pdf, &HashSet::from([1]), None).is_empty());
        assert!(
            pages_with_hidden_text_layer(&pdf, &HashSet::new(), Some(&HashSet::from([2])))
                .is_empty()
        );
        assert!(pages_with_hidden_text_layer(b"not a pdf", &HashSet::new(), None).is_empty());
    }

    #[test]
    fn scanning_is_bounded() {
        // A form that draws itself stops at the first repeat.
        let pdf = scan_pdf(
            &format!("{SCAN} /Fm1 Do {STAMP}"),
            &format!("{} /Fm1 Do", text_layer(3)),
        );
        assert_eq!(
            pages_with_hidden_text_layer(&pdf, &HashSet::new(), None),
            [1]
        );
        // Unbalanced saves and restores keep the state they can.
        let deep = "q ".repeat(MAX_SAVED_STATES + 10);
        let restores = "Q ".repeat(MAX_SAVED_STATES + 20);
        assert!(flagged(
            &format!("{SCAN} {deep}{restores}{} {STAMP}", text_layer(3)),
            ""
        ));
        let mut budget = Budget {
            bytes: MAX_CONTENT_BYTES,
            operations: 0,
        };
        assert!(budget.take_bytes(1).is_err());
        budget.operations = MAX_OPERATIONS;
        assert!(budget.take_operations(1).is_err());
    }
}
