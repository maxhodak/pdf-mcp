# pdf-mcp

An MCP server that gives AI agents tools to read, search, and extract figures from PDF files. Built in Rust on [MuPDF](https://mupdf.com/) for speed.

## Install

### Homebrew (macOS)

```
brew tap maxhodak/pdf-mcp https://github.com/maxhodak/pdf-mcp
brew install pdf-mcp
```

### From source

Requires a C compiler (Xcode CLI tools on macOS, `build-essential` on Linux).

```
cargo install --path .
```

### Prebuilt binaries

Download from [Releases](https://github.com/maxhodak/pdf-mcp/releases) for macOS (x86_64, aarch64) and Linux (x86_64).

## Usage

```json
{
  "mcpServers": {
    "pdf": {
      "command": "pdf-mcp"
    }
  }
}
```

Every tool takes a `path` argument pointing to any PDF on disk. The server caches the last opened document so repeated calls to the same file skip re-parsing.

## Tools

All coordinates are in PDF points (72pt = 1 inch, origin at top-left).

### `get_info`

Returns page count, title, author, subject, keywords, creator, producer, and per-page dimensions.

### `get_page_text`

Extracts all text from a page.

| Param | Required | Description |
|-------|----------|-------------|
| `page` | yes | Page number (0-indexed) |

### `search`

Finds all occurrences of a string and returns bounding boxes.

| Param | Required | Description |
|-------|----------|-------------|
| `query` | yes | Text to search for |
| `page` | no | Restrict to a single page |

### `render`

Renders a page or region as PNG/JPEG. Returns base64 inline or writes to disk.

| Param | Required | Description |
|-------|----------|-------------|
| `page` | yes | Page number (0-indexed) |
| `dpi` | no | Render resolution (default 150) |
| `x0`, `y0`, `x1`, `y1` | no | ROI crop in PDF points. Omit for full page. |
| `width` | no | Resize to this width (preserves aspect ratio) |
| `height` | no | Resize to this height (preserves aspect ratio) |
| `format` | no | `"png"` (default) or `"jpeg"` |
| `quality` | no | JPEG quality 1-100 (default 80) |
| `output_path` | no | Write to this file instead of returning inline |

When `output_path` is set, the response is a JSON summary:

```json
{"path": "/tmp/fig.jpg", "format": "jpeg", "width": 1200, "height": 800, "bytes": 94521}
```

### `get_text_blocks`

Returns text and image blocks with bounding boxes. Useful for understanding page layout before extracting specific regions.

| Param | Required | Description |
|-------|----------|-------------|
| `page` | yes | Page number (0-indexed) |

### `extract_region_text`

Extracts text from a rectangular region of a page.

| Param | Required | Description |
|-------|----------|-------------|
| `page` | yes | Page number (0-indexed) |
| `x0`, `y0`, `x1`, `y1` | yes | Region bounds in PDF points |

### `get_outline`

Returns the table of contents / bookmarks as a tree with page numbers.

### `get_page_links`

Returns all hyperlinks on a page with bounding boxes and URIs.

| Param | Required | Description |
|-------|----------|-------------|
| `page` | yes | Page number (0-indexed) |

### `intensity_profile`

Returns a 1D luminance profile along the vertical or horizontal axis. Each value (0-255) maps to one PDF point. Use to find content bounds and whitespace margins for smart cropping.

| Param | Required | Description |
|-------|----------|-------------|
| `page` | yes | Page number (0-indexed) |
| `axis` | yes | `"vertical"` (top-to-bottom) or `"horizontal"` (left-to-right) |
| `offset` | no | Sample at this perpendicular offset in PDF points |
| `band_width` | no | Width of band to average around offset (default 10 if offset given) |
| `threshold` | no | Luminance below this = content (default 250) |

Returns the profile array plus auto-detected `content_start` and `content_end` in PDF points.

## Typical agent workflow

1. `get_info` — learn page count and dimensions
2. `get_outline` — understand document structure
3. `get_page_text` — read content
4. `search` — find specific text, get bounding boxes
5. `get_text_blocks` — understand layout, find figures/tables
6. `render` — inspect a region visually or extract a figure
7. `render` with `output_path` — save the final image to disk

## License

AGPL-3.0 (inherited from MuPDF)
