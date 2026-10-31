//! Parse sidecars and check that rewriting them keeps their meaning and
//! everything Tintype doesn't own. Reads only; nothing is written.
//! `cargo run -p tintype-meta --example xmp-check -- file.xmp...`

use tintype_meta::xmp::{parse, update};

fn main() {
    let (mut ok, mut tags, mut people, mut rated) = (0, 0, 0, 0);
    for path in std::env::args().skip(1) {
        let xml = std::fs::read_to_string(&path).unwrap();
        let s = parse(&xml).unwrap_or_else(|e| panic!("{path}: {e}"));
        let rewritten = update(Some(&xml), &s).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(parse(&rewritten).unwrap(), s, "{path}: meaning changed");
        for attr in ["photostat:place=", "photostat:analysisHash=", "xpacket end"] {
            assert_eq!(xml.contains(attr), rewritten.contains(attr), "{path}: {attr} lost");
        }
        assert_eq!(update(Some(&rewritten), &s).unwrap(), rewritten, "{path}: not stable");
        ok += 1;
        tags += s.tags.len();
        people += s.people.len();
        rated += usize::from(s.rating.is_some());
    }
    println!("{ok} sidecars ok: {rated} rated, {tags} tags, {people} people names");
}
