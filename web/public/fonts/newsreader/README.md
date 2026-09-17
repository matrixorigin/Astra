# Newsreader

Bundled variable serif font used for Astra's English text rendering.

- Source: Google Fonts, `ofl/newsreader`
- Files:
  - `Newsreader-opsz-wght.ttf`
  - `Newsreader-Italic-opsz-wght.ttf`
- Axes used by the UI: `opsz`, `wght`
- License: SIL Open Font License 1.1, see `OFL.txt`

The application font declaration in `web/app/globals.css` intentionally
restricts `unicode-range` to Latin and common punctuation so Chinese glyphs
continue to render through the user's system CJK serif fonts.
