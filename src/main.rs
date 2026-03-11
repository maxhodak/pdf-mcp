use std::cell::RefCell;
use std::io::{self, BufRead, Cursor, Write};

use base64::Engine as _;
use mupdf::text_page::TextBlockType;
use mupdf::{Colorspace, Document, Matrix, MetadataName, Outline, TextPageFlags};
use serde_json::{json, Value};

trait StrErr<T> {
    fn str_err(self) -> Result<T, String>;
}
impl<T, E: std::fmt::Display> StrErr<T> for Result<T, E> {
    fn str_err(self) -> Result<T, String> {
        self.map_err(|e| e.to_string())
    }
}

/// Cache the last opened document so repeated calls to the same file are fast.
struct DocCache {
    path: String,
    doc: Document,
}

thread_local! {
    static CACHE: RefCell<Option<DocCache>> = const { RefCell::new(None) };
}

fn open_doc(path: &str) -> Result<(), String> {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if let Some(ref dc) = *cache {
            if dc.path == path {
                return Ok(());
            }
        }
        let doc = Document::open(path).map_err(|e| format!("Failed to open {path}: {e}"))?;
        *cache = Some(DocCache {
            path: path.to_string(),
            doc,
        });
        Ok(())
    })
}

fn with_doc<F, R>(path: &str, f: F) -> Result<R, String>
where
    F: FnOnce(&Document) -> Result<R, String>,
{
    let path = &std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    open_doc(path)?;
    CACHE.with(|c| {
        let cache = c.borrow();
        let dc = cache.as_ref().unwrap();
        f(&dc.doc)
    })
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                let resp = json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": {"code": -32700, "message": "Parse error"}
                });
                let mut out = stdout.lock();
                if serde_json::to_writer(&mut out, &resp).is_err()
                    || out.write_all(b"\n").is_err()
                    || out.flush().is_err()
                {
                    break;
                }
                continue;
            }
        };

        if let Some(resp) = handle_request(&req) {
            let mut out = stdout.lock();
            if serde_json::to_writer(&mut out, &resp).is_err()
                || out.write_all(b"\n").is_err()
                || out.flush().is_err()
            {
                break; // client disconnected
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MCP protocol
// ---------------------------------------------------------------------------

fn handle_request(req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();

    // Notifications (no id) get no response
    if id.is_none() {
        return None;
    }

    let method = match req.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32600, "message": "Invalid request: missing or non-string method"}
            }));
        }
    };
    let params = req.get("params").cloned().unwrap_or(json!({}));

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "pdf-mcp", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(tools_list()),
        "tools/call" => Ok(handle_tool_call(&params)),
        "ping" => Ok(json!({})),
        _ => Err(json!({"code": -32601, "message": format!("Method not found: {method}")})),
    };

    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

const PATH_PROP: &str = r#"{"type": "string", "description": "Absolute path to the PDF file"}"#;

fn tools_list() -> Value {
    let path_prop: Value = serde_json::from_str(PATH_PROP).unwrap();
    let tools: Vec<Value> = TOOL_DEFS.iter().map(|(name, desc, props, required)| {
        let mut properties: Value = serde_json::from_str(props).unwrap();
        properties.as_object_mut().unwrap().insert("path".into(), path_prop.clone());
        let mut req: Vec<Value> = serde_json::from_str(required).unwrap();
        req.insert(0, json!("path"));
        json!({
            "name": name,
            "description": desc,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": req,
            }
        })
    }).collect();
    json!({ "tools": tools })
}

/// (name, description, properties_json, required_json)
const TOOL_DEFS: &[(&str, &str, &str, &str)] = &[
    (
        "get_info",
        "Get PDF metadata: page count, title, author, subject, keywords, creator, producer, and per-page dimensions (in PDF points, 72pt = 1in).",
        "{}",
        "[]",
    ),
    (
        "get_page_text",
        "Extract all text content from a specific page.",
        r#"{"page": {"type": "integer", "description": "Page number (0-indexed)"}}"#,
        r#"["page"]"#,
    ),
    (
        "search",
        "Search for text in the PDF. Returns matches with page numbers and bounding boxes (in PDF points, 72pt = 1in, origin at top-left).",
        r#"{"query": {"type": "string", "description": "Text to search for"}, "page": {"type": "integer", "description": "Specific page (0-indexed). Omit to search all pages."}}"#,
        r#"["query"]"#,
    ),
    (
        "render",
        "Render a page (or rectangular region) as an image. Returns base64-encoded inline by default, or writes to disk if output_path is given. Coordinates in PDF points (72pt = 1in). Use search or get_text_blocks to find region coordinates.",
        concat!(
            r#"{"page": {"type": "integer", "description": "Page number (0-indexed)"},"#,
            r#" "dpi": {"type": "number", "description": "Render resolution in DPI (default 150). Higher = sharper but larger before resize."},"#,
            r#" "x0": {"type": "number", "description": "ROI left edge (PDF points). Omit x0/y0/x1/y1 to render full page."},"#,
            r#" "y0": {"type": "number", "description": "ROI top edge"},"#,
            r#" "x1": {"type": "number", "description": "ROI right edge"},"#,
            r#" "y1": {"type": "number", "description": "ROI bottom edge"},"#,
            r#" "width": {"type": "integer", "description": "Resize output to this width in pixels (preserves aspect ratio). Overrides height if both given."},"#,
            r#" "height": {"type": "integer", "description": "Resize output to this height in pixels (preserves aspect ratio)."},"#,
            r#" "format": {"type": "string", "enum": ["png", "jpeg"], "description": "Output format (default png). Use jpeg for smaller file size."},"#,
            r#" "quality": {"type": "integer", "description": "JPEG quality 1-100 (default 80). Ignored for PNG."},"#,
            r#" "output_path": {"type": "string", "description": "Write image to this file path instead of returning inline. Returns the path and file size on success."}}"#,
        ),
        r#"["page"]"#,
    ),
    (
        "get_text_blocks",
        "Get text organized into positioned blocks with bounding boxes. Useful for understanding page layout, locating figures, tables, and whitespace gaps.",
        r#"{"page": {"type": "integer", "description": "Page number (0-indexed)"}}"#,
        r#"["page"]"#,
    ),
    (
        "extract_region_text",
        "Extract text from a specific rectangular region of a page. Coordinates in PDF points.",
        r#"{"page": {"type": "integer", "description": "Page number (0-indexed)"}, "x0": {"type": "number"}, "y0": {"type": "number"}, "x1": {"type": "number"}, "y1": {"type": "number"}}"#,
        r#"["page", "x0", "y0", "x1", "y1"]"#,
    ),
    (
        "get_outline",
        "Get the PDF table of contents / bookmarks as a tree.",
        "{}",
        "[]",
    ),
    (
        "get_page_links",
        "Get all hyperlinks on a page with their bounding boxes and URIs.",
        r#"{"page": {"type": "integer", "description": "Page number (0-indexed)"}}"#,
        r#"["page"]"#,
    ),
    (
        "intensity_profile",
        "Get a 1D intensity profile along the vertical or horizontal axis of a page. Each value is mean luminance (0=black, 255=white) at that PDF-point coordinate. Use to find content bounds and whitespace for cropping. Returns profile array, auto-detected content_start/content_end in PDF points, and suggested crop bounds.",
        concat!(
            r#"{"page": {"type": "integer", "description": "Page number (0-indexed)"},"#,
            r#" "axis": {"type": "string", "enum": ["vertical", "horizontal"], "description": "vertical = profile top-to-bottom (find top/bottom margins), horizontal = profile left-to-right (find left/right margins)"},"#,
            r#" "offset": {"type": "number", "description": "Sample at this perpendicular offset in PDF points (e.g. for vertical axis, this is an x position). Omit to average across the full perpendicular dimension."},"#,
            r#" "band_width": {"type": "number", "description": "Width of band to average around offset, in PDF points (default 10 if offset given, full dimension if not)."},"#,
            r#" "threshold": {"type": "integer", "description": "Luminance below this value counts as content (default 250). Lower = stricter."}}"#,
        ),
        r#"["page", "axis"]"#,
    ),
];

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

fn handle_tool_call(params: &Value) -> Value {
    let name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => return tool_error("Missing tool name"),
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let path = match args.get("path").and_then(|p| p.as_str()) {
        Some(p) => p.to_string(),
        None => return tool_error("Missing required parameter: path"),
    };

    let r = match name {
        "get_info" => with_doc(&path, tool_get_info),
        "get_page_text" => with_doc(&path, |doc| tool_get_page_text(doc, &args)),
        "search" => with_doc(&path, |doc| tool_search(doc, &args)),
        "render" => with_doc(&path, |doc| tool_render(doc, &args)),
        "get_text_blocks" => with_doc(&path, |doc| tool_get_text_blocks(doc, &args)),
        "extract_region_text" => with_doc(&path, |doc| tool_extract_region_text(doc, &args)),
        "get_outline" => with_doc(&path, tool_get_outline),
        "get_page_links" => with_doc(&path, |doc| tool_get_page_links(doc, &args)),
        "intensity_profile" => with_doc(&path, |doc| tool_intensity_profile(doc, &args)),
        _ => Err(format!("Unknown tool: {name}")),
    };
    match r {
        Ok(v) => v,
        Err(e) => tool_error(&e),
    }
}

fn tool_error(msg: &str) -> Value {
    json!({"content": [{"type": "text", "text": msg}], "isError": true})
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn text_content(s: &str) -> Value {
    json!({"content": [{"type": "text", "text": s}]})
}

fn json_content(v: &Value) -> Value {
    text_content(&serde_json::to_string_pretty(v).unwrap())
}

fn image_content(data: &[u8], mime: &str) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    json!({"content": [{"type": "image", "data": b64, "mimeType": mime}]})
}

fn page_num(args: &Value) -> Result<i32, String> {
    args.get("page")
        .and_then(|p| p.as_i64())
        .map(|p| p as i32)
        .ok_or_else(|| "Missing required parameter: page".into())
}

fn float_arg(args: &Value, key: &str) -> Result<f32, String> {
    args.get(key)
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
        .ok_or_else(|| format!("Missing required parameter: {key}"))
}

fn rect_json(x0: f32, y0: f32, x1: f32, y1: f32) -> Value {
    json!({"x0": x0, "y0": y0, "x1": x1, "y1": y1})
}

fn quad_bbox(q: &mupdf::Quad) -> (f32, f32, f32, f32) {
    let x0 = q.ul.x.min(q.ll.x);
    let y0 = q.ul.y.min(q.ur.y);
    let x1 = q.ur.x.max(q.lr.x);
    let y1 = q.ll.y.max(q.lr.y);
    (x0, y0, x1, y1)
}

// ---------------------------------------------------------------------------
// Tool: get_info
// ---------------------------------------------------------------------------

fn tool_get_info(doc: &Document) -> Result<Value, String> {
    let pc = doc.page_count().str_err()?;

    let meta = |name: MetadataName| doc.metadata(name).unwrap_or_default();

    let mut pages = Vec::new();
    for i in 0..pc {
        if let Ok(page) = doc.load_page(i) {
            if let Ok(b) = page.bounds() {
                pages.push(json!({
                    "page": i,
                    "width": b.x1 - b.x0,
                    "height": b.y1 - b.y0,
                }));
            }
        }
    }

    let info = json!({
        "page_count": pc,
        "title": meta(MetadataName::Title),
        "author": meta(MetadataName::Author),
        "subject": meta(MetadataName::Subject),
        "keywords": meta(MetadataName::Keywords),
        "creator": meta(MetadataName::Creator),
        "producer": meta(MetadataName::Producer),
        "pages": pages,
    });
    Ok(json_content(&info))
}

// ---------------------------------------------------------------------------
// Tool: get_page_text
// ---------------------------------------------------------------------------

fn tool_get_page_text(doc: &Document, args: &Value) -> Result<Value, String> {
    let pn = page_num(args)?;
    let page = doc.load_page(pn).str_err()?;
    let tp = page
        .to_text_page(TextPageFlags::empty())
        .str_err()?;
    let text = tp.to_text().str_err()?;
    Ok(text_content(&text))
}

// ---------------------------------------------------------------------------
// Tool: search
// ---------------------------------------------------------------------------

fn tool_search(doc: &Document, args: &Value) -> Result<Value, String> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .ok_or("Missing required parameter: query")?;
    let specific = args.get("page").and_then(|p| p.as_i64()).map(|p| p as i32);

    let pc = doc.page_count().str_err()?;
    let range: Vec<i32> = match specific {
        Some(p) => vec![p],
        None => (0..pc).collect(),
    };

    let mut matches = Vec::new();
    let mut truncated = false;
    for pn in range {
        let page = doc.load_page(pn).str_err()?;
        let hits = page.search(query, 500).str_err()?;
        if hits.len() >= 500 {
            truncated = true;
        }
        for q in hits.iter() {
            let (x0, y0, x1, y1) = quad_bbox(q);
            matches.push(json!({
                "page": pn,
                "bbox": rect_json(x0, y0, x1, y1),
            }));
        }
    }

    let mut result = json!({
        "query": query,
        "match_count": matches.len(),
        "matches": matches,
    });
    if truncated {
        result["truncated"] = json!(true);
    }
    Ok(json_content(&result))
}

// ---------------------------------------------------------------------------
// Tool: render
// ---------------------------------------------------------------------------

fn tool_render(doc: &Document, args: &Value) -> Result<Value, String> {
    let pn = page_num(args)?;
    let dpi = args.get("dpi").and_then(|d| d.as_f64()).unwrap_or(150.0).min(1200.0) as f32;
    let scale = dpi / 72.0;

    // Render full page at requested DPI
    let page = doc.load_page(pn).str_err()?;
    let ctm = Matrix::new_scale(scale, scale);
    let pix = page
        .to_pixmap(&ctm, &Colorspace::device_rgb(), false, false)
        .str_err()?;

    let pw = pix.width();
    let ph = pix.height();
    let n = pix.n() as u32;
    if n < 3 {
        return Err(format!("Unexpected pixel format: {n} channels (expected >= 3)"));
    }
    let stride = pix.stride() as u32;
    let samples = pix.samples();

    // Optional ROI crop (PDF points -> pixels)
    let has_roi = args.get("x0").is_some();
    let (px0, py0, cw, ch) = if has_roi {
        let x0 = float_arg(args, "x0")?;
        let y0 = float_arg(args, "y0")?;
        let x1 = float_arg(args, "x1")?;
        let y1 = float_arg(args, "y1")?;
        let px0 = ((x0 * scale) as u32).min(pw);
        let py0 = ((y0 * scale) as u32).min(ph);
        let px1 = ((x1 * scale) as u32).min(pw);
        let py1 = ((y1 * scale) as u32).min(ph);
        let cw = px1.saturating_sub(px0);
        let ch = py1.saturating_sub(py0);
        if cw == 0 || ch == 0 {
            return Err("Region has zero area".into());
        }
        (px0, py0, cw, ch)
    } else {
        (0u32, 0u32, pw, ph)
    };

    // Extract RGB pixels for the (possibly cropped) region
    let mut rgb = Vec::with_capacity((cw * ch * 3) as usize);
    for row in py0..(py0 + ch) {
        let row_start = (row * stride) as usize;
        for col in px0..(px0 + cw) {
            let off = row_start + (col * n) as usize;
            rgb.push(samples[off]);
            rgb.push(samples[off + 1]);
            rgb.push(samples[off + 2]);
        }
    }

    let mut img: image::DynamicImage =
        image::RgbImage::from_raw(cw, ch, rgb).ok_or("Failed to build image")?.into();

    // Resize if requested
    let target_w = args.get("width").and_then(|v| v.as_u64()).map(|v| (v as u32).max(1));
    let target_h = args.get("height").and_then(|v| v.as_u64()).map(|v| (v as u32).max(1));
    if let Some((tw, th)) = resize_dims(img.width(), img.height(), target_w, target_h) {
        img = img.resize(tw, th, image::imageops::FilterType::Lanczos3);
    }

    // Encode to requested format
    let format_str = args
        .get("format")
        .and_then(|f| f.as_str())
        .unwrap_or("png");
    let (encoded, mime) = encode_image(&img, format_str, args)?;

    // Write to disk or return inline
    if let Some(output_path) = args.get("output_path").and_then(|p| p.as_str()) {
        std::fs::write(output_path, &encoded)
            .map_err(|e| format!("Failed to write {output_path}: {e}"))?;
        Ok(json_content(&json!({
            "path": output_path,
            "format": format_str,
            "width": img.width(),
            "height": img.height(),
            "bytes": encoded.len(),
        })))
    } else {
        Ok(image_content(&encoded, mime))
    }
}

/// Compute target (w, h) preserving aspect ratio. Returns None if no resize needed.
fn resize_dims(w: u32, h: u32, tw: Option<u32>, th: Option<u32>) -> Option<(u32, u32)> {
    match (tw, th) {
        (Some(tw), _) => {
            let scale = tw as f64 / w as f64;
            Some((tw, (h as f64 * scale).round() as u32))
        }
        (None, Some(th)) => {
            let scale = th as f64 / h as f64;
            Some(((w as f64 * scale).round() as u32, th))
        }
        (None, None) => None,
    }
}

fn encode_image(img: &image::DynamicImage, format: &str, args: &Value) -> Result<(Vec<u8>, &'static str), String> {
    let mut buf = Vec::new();
    match format {
        "jpeg" | "jpg" => {
            let quality = args.get("quality").and_then(|q| q.as_u64()).unwrap_or(80) as u8;
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
            img.write_with_encoder(encoder).str_err()?;
            Ok((buf, "image/jpeg"))
        }
        _ => {
            img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                .str_err()?;
            Ok((buf, "image/png"))
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: get_text_blocks
// ---------------------------------------------------------------------------

fn tool_get_text_blocks(doc: &Document, args: &Value) -> Result<Value, String> {
    let pn = page_num(args)?;
    let page = doc.load_page(pn).str_err()?;
    let tp = page
        .to_text_page(TextPageFlags::empty())
        .str_err()?;

    let mut blocks = Vec::new();
    for block in tp.blocks() {
        let bounds = block.bounds();
        let btype = block.r#type();
        match btype {
            TextBlockType::Text => {
                let mut text = String::new();
                for line in block.lines() {
                    for ch in line.chars() {
                        if let Some(c) = ch.char() {
                            text.push(c);
                        }
                    }
                    text.push('\n');
                }
                blocks.push(json!({
                    "type": "text",
                    "bbox": rect_json(bounds.x0, bounds.y0, bounds.x1, bounds.y1),
                    "text": text.trim_end(),
                }));
            }
            TextBlockType::Image => {
                blocks.push(json!({
                    "type": "image",
                    "bbox": rect_json(bounds.x0, bounds.y0, bounds.x1, bounds.y1),
                }));
            }
            _ => {
                blocks.push(json!({
                    "type": format!("{:?}", btype),
                    "bbox": rect_json(bounds.x0, bounds.y0, bounds.x1, bounds.y1),
                }));
            }
        }
    }

    Ok(json_content(&json!({
        "page": pn,
        "block_count": blocks.len(),
        "blocks": blocks,
    })))
}

// ---------------------------------------------------------------------------
// Tool: extract_region_text
// ---------------------------------------------------------------------------

fn tool_extract_region_text(doc: &Document, args: &Value) -> Result<Value, String> {
    let pn = page_num(args)?;
    let rx0 = float_arg(args, "x0")?;
    let ry0 = float_arg(args, "y0")?;
    let rx1 = float_arg(args, "x1")?;
    let ry1 = float_arg(args, "y1")?;

    let page = doc.load_page(pn).str_err()?;
    let tp = page
        .to_text_page(TextPageFlags::empty())
        .str_err()?;

    let mut text = String::new();
    for block in tp.blocks() {
        if block.r#type() != TextBlockType::Text {
            continue;
        }
        for line in block.lines() {
            let mut line_text = String::new();
            let mut any = false;
            for ch in line.chars() {
                let p = ch.origin();
                if p.x >= rx0 && p.x <= rx1 && p.y >= ry0 && p.y <= ry1 {
                    if let Some(c) = ch.char() {
                        line_text.push(c);
                        any = true;
                    }
                }
            }
            if any {
                text.push_str(&line_text);
                text.push('\n');
            }
        }
    }
    Ok(text_content(text.trim()))
}

// ---------------------------------------------------------------------------
// Tool: get_outline
// ---------------------------------------------------------------------------

fn tool_get_outline(doc: &Document) -> Result<Value, String> {
    let outlines = doc.outlines().str_err()?;

    fn walk(items: &[Outline], depth: usize) -> Vec<Value> {
        items
            .iter()
            .map(|item| {
                let mut e = json!({
                    "title": item.title,
                    "uri": item.uri,
                });
                if let Some(ref dest) = item.dest {
                    e["page"] = json!(dest.loc.page_number);
                }
                if !item.down.is_empty() && depth < 100 {
                    e["children"] = json!(walk(&item.down, depth + 1));
                }
                e
            })
            .collect()
    }

    let toc = json!(walk(&outlines, 0));
    Ok(json_content(&toc))
}

// ---------------------------------------------------------------------------
// Tool: get_page_links
// ---------------------------------------------------------------------------

fn tool_get_page_links(doc: &Document, args: &Value) -> Result<Value, String> {
    let pn = page_num(args)?;
    let page = doc.load_page(pn).str_err()?;
    let links_iter = page.links().str_err()?;

    let mut links = Vec::new();
    for link in links_iter {
        let b = link.bounds;
        links.push(json!({
            "uri": link.uri,
            "bbox": rect_json(b.x0, b.y0, b.x1, b.y1),
        }));
    }

    Ok(json_content(&json!({
        "page": pn,
        "link_count": links.len(),
        "links": links,
    })))
}

// ---------------------------------------------------------------------------
// Tool: intensity_profile
// ---------------------------------------------------------------------------

fn tool_intensity_profile(doc: &Document, args: &Value) -> Result<Value, String> {
    let pn = page_num(args)?;
    let axis = args
        .get("axis")
        .and_then(|a| a.as_str())
        .ok_or("Missing required parameter: axis")?;
    let threshold = args
        .get("threshold")
        .and_then(|t| t.as_u64())
        .unwrap_or(250) as u8;

    // Render at 72 DPI → 1 pixel = 1 PDF point, fast and maps directly
    let page = doc.load_page(pn).str_err()?;
    let pix = page
        .to_pixmap(
            &Matrix::new_scale(1.0, 1.0),
            &Colorspace::device_rgb(),
            false,
            false,
        )
        .str_err()?;

    let pw = pix.width() as usize;
    let ph = pix.height() as usize;
    let n = pix.n() as usize;
    if n < 3 {
        return Err(format!("Unexpected pixel format: {n} channels (expected >= 3)"));
    }
    let stride = pix.stride() as usize;
    let samples = pix.samples();

    // Luminance at pixel (col, row)
    let lum = |col: usize, row: usize| -> u8 {
        let off = row * stride + col * n;
        let r = samples[off] as u32;
        let g = samples[off + 1] as u32;
        let b = samples[off + 2] as u32;
        ((r * 299 + g * 587 + b * 114) / 1000) as u8
    };

    let (profile, length) = match axis {
        "vertical" => {
            // Profile runs top-to-bottom (one value per y).
            // Perpendicular dimension is x. offset = x position.
            let (col_lo, col_hi) = perp_range(args, pw)?;
            let span = (col_hi - col_lo).max(1);
            let mut prof = Vec::with_capacity(ph);
            for row in 0..ph {
                let sum: u64 = (col_lo..col_hi).map(|col| lum(col, row) as u64).sum();
                prof.push((sum / span as u64) as u8);
            }
            (prof, ph)
        }
        "horizontal" => {
            // Profile runs left-to-right (one value per x).
            // Perpendicular dimension is y. offset = y position.
            let (row_lo, row_hi) = perp_range(args, ph)?;
            let span = (row_hi - row_lo).max(1);
            let mut prof = Vec::with_capacity(pw);
            for col in 0..pw {
                let sum: u64 = (row_lo..row_hi).map(|row| lum(col, row) as u64).sum();
                prof.push((sum / span as u64) as u8);
            }
            (prof, pw)
        }
        _ => return Err(format!("axis must be \"vertical\" or \"horizontal\", got \"{axis}\"")),
    };

    // Find content bounds
    let content_start = profile.iter().position(|&v| v < threshold);
    let content_end = profile.iter().rposition(|&v| v < threshold);

    let result = json!({
        "axis": axis,
        "page": pn,
        "length": length,
        "threshold": threshold,
        "content_start": content_start,
        "content_end": content_end.map(|e| e + 1),
        "profile": profile,
    });
    Ok(json_content(&result))
}

/// Compute the perpendicular pixel range [lo, hi) from offset/band_width args.
fn perp_range(args: &Value, dim: usize) -> Result<(usize, usize), String> {
    if let Some(offset) = args.get("offset").and_then(|o| o.as_f64()) {
        let band = args
            .get("band_width")
            .and_then(|b| b.as_f64())
            .unwrap_or(10.0);
        let center = (offset.max(0.0) as usize).min(dim.saturating_sub(1));
        let half = (band / 2.0).max(0.5) as usize;
        let lo = center.saturating_sub(half).min(dim);
        let hi = (center + half + 1).min(dim);
        Ok((lo, hi))
    } else {
        Ok((0, dim))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_pdf() -> String {
        format!("{}/tests/fixtures/test.pdf", env!("CARGO_MANIFEST_DIR"))
    }

    /// Open the test PDF directly (bypasses CACHE thread-local to avoid
    /// TLS destruction order issues with mupdf's internal context).
    fn open_test_doc() -> Document {
        Document::open(&test_pdf()).unwrap()
    }

    // -----------------------------------------------------------------------
    // Pure unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resize_dims_width_only() {
        assert_eq!(resize_dims(100, 200, Some(50), None), Some((50, 100)));
    }

    #[test]
    fn test_resize_dims_height_only() {
        assert_eq!(resize_dims(100, 200, None, Some(100)), Some((50, 100)));
    }

    #[test]
    fn test_resize_dims_width_takes_priority() {
        assert_eq!(resize_dims(100, 200, Some(50), Some(999)), Some((50, 100)));
    }

    #[test]
    fn test_resize_dims_none() {
        assert_eq!(resize_dims(100, 200, None, None), None);
    }

    #[test]
    fn test_resize_dims_upscale() {
        assert_eq!(resize_dims(10, 20, Some(100), None), Some((100, 200)));
    }

    #[test]
    fn test_perp_range_no_offset() {
        let args = json!({});
        assert_eq!(perp_range(&args, 100).unwrap(), (0, 100));
    }

    #[test]
    fn test_perp_range_with_offset() {
        let args = json!({"offset": 50.0});
        let (lo, hi) = perp_range(&args, 100).unwrap();
        assert!(lo < 50 && hi > 50);
    }

    #[test]
    fn test_perp_range_offset_at_zero() {
        let args = json!({"offset": 0.0, "band_width": 10.0});
        let (lo, _hi) = perp_range(&args, 100).unwrap();
        assert_eq!(lo, 0);
    }

    #[test]
    fn test_perp_range_negative_offset() {
        let args = json!({"offset": -10.0});
        let (lo, _hi) = perp_range(&args, 100).unwrap();
        assert_eq!(lo, 0);
    }

    #[test]
    fn test_perp_range_beyond_dim() {
        let args = json!({"offset": 500.0, "band_width": 10.0});
        let (_lo, hi) = perp_range(&args, 100).unwrap();
        assert_eq!(hi, 100);
    }

    #[test]
    fn test_tool_error_shape() {
        let v = tool_error("something broke");
        assert_eq!(v["isError"], true);
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "something broke");
    }

    #[test]
    fn test_text_content_shape() {
        let v = text_content("hello");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello");
    }

    #[test]
    fn test_rect_json() {
        let r = rect_json(1.0, 2.0, 3.0, 4.0);
        assert_eq!(r["x0"], 1.0);
        assert_eq!(r["y0"], 2.0);
        assert_eq!(r["x1"], 3.0);
        assert_eq!(r["y1"], 4.0);
    }

    #[test]
    fn test_page_num_present() {
        let args = json!({"page": 5});
        assert_eq!(page_num(&args).unwrap(), 5);
    }

    #[test]
    fn test_page_num_missing() {
        let args = json!({});
        assert!(page_num(&args).is_err());
    }

    #[test]
    fn test_float_arg_present() {
        let args = json!({"x": 3.14});
        assert!((float_arg(&args, "x").unwrap() - 3.14).abs() < 0.01);
    }

    #[test]
    fn test_float_arg_missing() {
        let args = json!({});
        assert!(float_arg(&args, "x").is_err());
    }

    // -----------------------------------------------------------------------
    // Protocol tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_notification_no_response() {
        let req = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_request(&req).is_none());
    }

    #[test]
    fn test_missing_method() {
        let req = json!({"jsonrpc": "2.0", "id": 1});
        let resp = handle_request(&req).unwrap();
        assert_eq!(resp["error"]["code"], -32600);
    }

    #[test]
    fn test_unknown_method() {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "bogus"});
        let resp = handle_request(&req).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn test_initialize() {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        let resp = handle_request(&req).unwrap();
        let result = &resp["result"];
        assert_eq!(result["serverInfo"]["name"], "pdf-mcp");
        assert!(result["protocolVersion"].is_string());
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn test_ping() {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}});
        let resp = handle_request(&req).unwrap();
        assert!(resp["result"].is_object());
        assert!(resp["error"].is_null());
    }

    #[test]
    fn test_tools_list_count() {
        let list = tools_list();
        let tools = list["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 9);
    }

    #[test]
    fn test_tools_list_all_have_path() {
        let list = tools_list();
        for tool in list["tools"].as_array().unwrap() {
            let props = &tool["inputSchema"]["properties"];
            assert!(props["path"].is_object(), "tool {} missing path prop", tool["name"]);
            let req = tool["inputSchema"]["required"].as_array().unwrap();
            assert!(req.contains(&json!("path")), "tool {} missing path in required", tool["name"]);
        }
    }

    #[test]
    fn test_tool_call_missing_name() {
        let params = json!({"arguments": {"path": "/tmp/x.pdf"}});
        let resp = handle_tool_call(&params);
        assert_eq!(resp["isError"], true);
    }

    #[test]
    fn test_tool_call_missing_path() {
        let params = json!({"name": "get_info", "arguments": {}});
        let resp = handle_tool_call(&params);
        assert_eq!(resp["isError"], true);
    }

    #[test]
    fn test_tool_call_unknown_tool() {
        let params = json!({"name": "bogus", "arguments": {"path": "/tmp/x.pdf"}});
        let resp = handle_tool_call(&params);
        assert_eq!(resp["isError"], true);
    }

    // -----------------------------------------------------------------------
    // Integration tests (require test PDF)
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_info() {
        let doc = open_test_doc();
        let result = tool_get_info(&doc).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let info: Value = serde_json::from_str(text).unwrap();
        assert_eq!(info["page_count"], 1);
        assert_eq!(info["pages"][0]["width"], 612.0);
        assert_eq!(info["pages"][0]["height"], 792.0);
        // No PII in metadata
        assert_eq!(info["author"], "");
    }

    #[test]
    fn test_get_page_text() {
        let doc = open_test_doc();
        let args = json!({"page": 0});
        let result = tool_get_page_text(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Hello World"));
        assert!(text.contains("Test PDF for pdf-mcp"));
    }

    #[test]
    fn test_get_page_text_invalid_page() {
        let doc = open_test_doc();
        let args = json!({"page": 999});
        assert!(tool_get_page_text(&doc, &args).is_err());
    }

    #[test]
    fn test_search_found() {
        let doc = open_test_doc();
        let args = json!({"query": "Hello"});
        let result = tool_search(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["match_count"], 1);
        assert!(data["matches"][0]["bbox"]["x0"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn test_search_not_found() {
        let doc = open_test_doc();
        let args = json!({"query": "ZZZZNOTFOUND"});
        let result = tool_search(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["match_count"], 0);
    }

    #[test]
    fn test_search_specific_page() {
        let doc = open_test_doc();
        let args = json!({"query": "Hello", "page": 0});
        let result = tool_search(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["match_count"], 1);
    }

    #[test]
    fn test_render_png() {
        let doc = open_test_doc();
        let args = json!({"page": 0});
        let result = tool_render(&doc, &args).unwrap();
        assert_eq!(result["content"][0]["type"], "image");
        assert_eq!(result["content"][0]["mimeType"], "image/png");
        let data = result["content"][0]["data"].as_str().unwrap();
        assert!(!data.is_empty());
    }

    #[test]
    fn test_render_jpeg() {
        let doc = open_test_doc();
        let args = json!({"page": 0, "format": "jpeg", "quality": 50});
        let result = tool_render(&doc, &args).unwrap();
        assert_eq!(result["content"][0]["mimeType"], "image/jpeg");
    }

    #[test]
    fn test_render_with_resize() {
        let doc = open_test_doc();
        let args = json!({"page": 0, "width": 100});
        let result = tool_render(&doc, &args).unwrap();
        assert_eq!(result["content"][0]["type"], "image");
    }

    #[test]
    fn test_render_with_roi() {
        let doc = open_test_doc();
        let args = json!({"page": 0, "x0": 50, "y0": 50, "x1": 200, "y1": 200});
        let result = tool_render(&doc, &args).unwrap();
        assert_eq!(result["content"][0]["type"], "image");
    }

    #[test]
    fn test_render_zero_area_roi() {
        let doc = open_test_doc();
        let args = json!({"page": 0, "x0": 100, "y0": 100, "x1": 100, "y1": 100});
        assert!(tool_render(&doc, &args).is_err());
    }

    #[test]
    fn test_render_to_file() {
        let doc = open_test_doc();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.png");
        let args = json!({"page": 0, "output_path": out.to_str().unwrap()});
        let result = tool_render(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["format"], "png");
        assert!(data["bytes"].as_u64().unwrap() > 0);
        assert!(out.exists());
    }

    #[test]
    fn test_render_dpi_capped() {
        let doc = open_test_doc();
        // DPI of 10000 should be clamped to 1200, not OOM
        let args = json!({"page": 0, "dpi": 10000, "width": 100});
        let result = tool_render(&doc, &args).unwrap();
        assert_eq!(result["content"][0]["type"], "image");
    }

    #[test]
    fn test_get_text_blocks() {
        let doc = open_test_doc();
        let args = json!({"page": 0});
        let result = tool_get_text_blocks(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert!(data["block_count"].as_u64().unwrap() > 0);
        let blocks = data["blocks"].as_array().unwrap();
        assert!(blocks.iter().any(|b| b["type"] == "text"));
    }

    #[test]
    fn test_extract_region_text() {
        let doc = open_test_doc();
        // Full page region should capture all text
        let args = json!({"page": 0, "x0": 0, "y0": 0, "x1": 612, "y1": 792});
        let result = tool_extract_region_text(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Hello World"));
    }

    #[test]
    fn test_extract_region_text_empty_region() {
        let doc = open_test_doc();
        // Region with no text
        let args = json!({"page": 0, "x0": 0, "y0": 0, "x1": 10, "y1": 10});
        let result = tool_extract_region_text(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.is_empty());
    }

    #[test]
    fn test_get_outline() {
        let doc = open_test_doc();
        let result = tool_get_outline(&doc).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert!(data.is_array());
    }

    #[test]
    fn test_get_page_links() {
        let doc = open_test_doc();
        let args = json!({"page": 0});
        let result = tool_get_page_links(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["page"], 0);
        assert!(data["links"].is_array());
    }

    #[test]
    fn test_intensity_profile_vertical() {
        let doc = open_test_doc();
        let args = json!({"page": 0, "axis": "vertical"});
        let result = tool_intensity_profile(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["axis"], "vertical");
        assert_eq!(data["length"], 792);
        let profile = data["profile"].as_array().unwrap();
        assert_eq!(profile.len(), 792);
        assert!(data["content_start"].is_number());
        assert!(data["content_end"].is_number());
    }

    #[test]
    fn test_intensity_profile_horizontal() {
        let doc = open_test_doc();
        let args = json!({"page": 0, "axis": "horizontal"});
        let result = tool_intensity_profile(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["axis"], "horizontal");
        assert_eq!(data["length"], 612);
    }

    #[test]
    fn test_intensity_profile_invalid_axis() {
        let doc = open_test_doc();
        let args = json!({"page": 0, "axis": "diagonal"});
        assert!(tool_intensity_profile(&doc, &args).is_err());
    }

    #[test]
    fn test_intensity_profile_with_offset() {
        let doc = open_test_doc();
        let args = json!({"page": 0, "axis": "vertical", "offset": 300.0, "band_width": 20.0});
        let result = tool_intensity_profile(&doc, &args).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["length"], 792);
    }
}
