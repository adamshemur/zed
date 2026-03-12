# pdf_viewer crate

Native PDF rendering for Zed using [hayro](https://crates.io/crates/hayro) (pure Rust, no C dependencies).

## Architecture

- `PdfItem` — project model that loads PDF bytes and parses via `hayro::Pdf`
- `PdfView` — the UI view with scroll-based page tracking, zoom, and page navigation
- `render_pdf_page()` — renders a single page to an RGBA image on a background thread
- Pages render asynchronously and display "Loading page N..." placeholders until ready

## Key patterns

- Follows the same `ProjectItem` / `Item` / `ProjectItem` trait pattern as `image_viewer`
- Uses `ScrollHandle` for scroll state; page tracking is derived from scroll offset + page height accumulation
- BGRA→RGBA pixel swap is needed because hayro outputs BGRA but GPUI expects RGBA

## Upstream status

Zed team has no plans to add PDF viewing to core (see [discussion #47094](https://github.com/zed-industries/zed/discussions/47094)). Extension API doesn't support custom views yet. This stays as a fork feature.
