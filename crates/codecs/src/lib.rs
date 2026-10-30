//! Image decoding: thumbnails, oriented previews and RAW embedded JPEGs.
//!
//! Planned: `image` + `zune-jpeg` for common formats, `rawler` for RAW
//! previews, `libheif` (statically linked) for HEIC. A disk thumbnail cache
//! keyed by catalog hash so moved files keep their thumbnails.
