//! Stacking: grouping copies, versions and burst frames of one shot.
//!
//! Tuned on a real library (4,253 Fujifilm photos, RAF + JPG):
//! - Exact copies share a content hash.
//! - Versions of one shot (RAF + JPG, edits, exports) keep the camera's
//!   capture time, so the same camera model and the same second is the same
//!   shot, whatever the edit did to the pixels (black-and-white exports of a
//!   RAF scored as low as 0.85). Exports may drop the serial number, so
//!   serials only keep apart two bodies when both files have one.
//! - Burst frames are taken by the same body within seconds and look alike:
//!   at 3-10 s apart and 0.93+ similarity, pairs were almost all bursts;
//!   below that, mostly reframed shots. Repeats of one frame up to a minute
//!   apart scored 0.97+; at 0.955-0.97 they were mostly reframed or zoomed.
//! - Looks alone never stack photos taken at different times: the same view
//!   at sunrise and sunset scored 0.975. Only photos with no capture time
//!   (stripped exports) are matched by looks alone, and strictly.
//!
//! The cover (the photo shown for the stack) is the highest rated, then the
//! largest, then a non-raw file, then the most recently changed.

use std::collections::HashMap;

use tintype_catalog::{Catalog, StackCandidate, StackPlace};

/// Frames at most this far apart can be a burst...
const BURST_GAP_S: i64 = 10;
/// ...if they look this alike...
const BURST_SIMILARITY: f32 = 0.93;
/// Frames up to this far apart are repeats of one shot...
const REPEAT_GAP_S: i64 = 60;
/// ...if they look this alike.
const REPEAT_SIMILARITY: f32 = 0.97;
/// A stack of bursts and repeats spans no more than this.
const BURST_SPAN_S: i64 = 60;
/// Photos without a capture time stack with a photo that looks this alike.
const UNDATED_SIMILARITY: f32 = 0.96;

/// Bump when the rules change; libraries are then restacked on open.
pub const STACKS_VERSION: u32 = 3;

struct Groups {
    parent: Vec<usize>,
    /// Earliest and latest capture time in each group (at its root).
    span: Vec<Option<(i64, i64)>>,
    /// The body serial of each group (at its root), if any member has one.
    serial: Vec<Option<String>>,
}

impl Groups {
    fn new(photos: &[StackCandidate]) -> Self {
        Self {
            parent: (0..photos.len()).collect(),
            span: photos.iter().map(|p| p.taken.map(|t| (t, t))).collect(),
            serial: photos.iter().map(|p| p.serial.clone()).collect(),
        }
    }

    fn root(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    fn merged_span(&mut self, a: usize, b: usize) -> Option<(i64, i64)> {
        let (ra, rb) = (self.root(a), self.root(b));
        match (self.span[ra], self.span[rb]) {
            (Some((a0, a1)), Some((b0, b1))) => Some((a0.min(b0), a1.max(b1))),
            (s, None) | (None, s) => s,
        }
    }

    /// Joins the groups of `a` and `b`, unless that would make a group span
    /// more than `max_span` or mix two camera bodies.
    fn join(&mut self, a: usize, b: usize, max_span: Option<i64>) {
        let span = self.merged_span(a, b);
        if let (Some(max), Some((lo, hi))) = (max_span, span)
            && hi - lo > max
        {
            return;
        }
        let (ra, rb) = (self.root(a), self.root(b));
        if ra == rb {
            return;
        }
        if let (Some(sa), Some(sb)) = (&self.serial[ra], &self.serial[rb])
            && sa != sb
        {
            return;
        }
        self.parent[rb] = ra;
        self.span[ra] = span;
        if self.serial[ra].is_none() {
            self.serial[ra] = self.serial[rb].take();
        }
    }
}

/// Two photos could come from the same body.
fn same_body(a: &StackCandidate, b: &StackCandidate) -> bool {
    a.camera.is_some() && a.camera == b.camera && (a.serial.is_none() || b.serial.is_none() || a.serial == b.serial)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Groups `photos` into stacks of two or more; `embeddings` (from the
/// analysis pass) may be missing for some or all photos.
pub fn group(photos: &[StackCandidate], embeddings: &HashMap<i64, Vec<f32>>) -> HashMap<i64, StackPlace> {
    let mut groups = Groups::new(photos);
    let emb = |i: usize| embeddings.get(&photos[i].id).map(Vec::as_slice);

    // Exact copies, and versions of one shot.
    let mut first_by_hash: HashMap<&str, usize> = HashMap::new();
    let mut by_shot: HashMap<(&str, i64), Vec<usize>> = HashMap::new();
    for (i, p) in photos.iter().enumerate() {
        if let Some(h) = p.hash.as_deref() {
            let first = *first_by_hash.entry(h).or_insert(i);
            groups.join(first, i, None);
        }
        if let (Some(camera), Some(t)) = (p.camera.as_deref(), p.taken) {
            let shot = by_shot.entry((camera, t)).or_default();
            if let Some(&other) = shot.iter().find(|&&o| same_body(&photos[o], p)) {
                groups.join(other, i, None);
            }
            shot.push(i);
        }
    }

    // Bursts: same body, a few seconds apart, alike.
    let mut timed: Vec<usize> =
        (0..photos.len()).filter(|&i| photos[i].camera.is_some() && photos[i].taken.is_some()).collect();
    timed.sort_by_key(|&i| (photos[i].camera.clone(), photos[i].taken));
    for (k, &a) in timed.iter().enumerate() {
        let Some(ea) = emb(a) else { continue };
        for &b in &timed[k + 1..] {
            let (pa, pb) = (&photos[a], &photos[b]);
            let gap = pb.taken.unwrap_or(0) - pa.taken.unwrap_or(0);
            if pb.camera != pa.camera || gap > REPEAT_GAP_S {
                break;
            }
            let needed = if gap <= BURST_GAP_S { BURST_SIMILARITY } else { REPEAT_SIMILARITY };
            if gap > 0
                && same_body(pa, pb)
                && let Some(eb) = emb(b)
                && dot(ea, eb) >= needed
            {
                groups.join(a, b, Some(BURST_SPAN_S));
            }
        }
    }

    // Undated photos (exports with metadata stripped): by looks alone, strictly.
    let undated: Vec<usize> = (0..photos.len()).filter(|&i| photos[i].taken.is_none()).collect();
    for &a in &undated {
        let Some(ea) = emb(a) else { continue };
        let best = (0..photos.len())
            .filter(|&b| b != a)
            .filter_map(|b| emb(b).map(|eb| (dot(ea, eb), b)))
            .max_by(|x, y| x.0.total_cmp(&y.0));
        if let Some((sim, b)) = best
            && sim >= UNDATED_SIMILARITY
        {
            groups.join(a, b, None);
        }
    }

    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..photos.len() {
        let r = groups.root(i);
        members.entry(r).or_default().push(i);
    }
    let mut places = HashMap::new();
    for m in members.into_values().filter(|m| m.len() > 1) {
        let stack = m.iter().map(|&i| photos[i].id).min().expect("not empty");
        let cover = cover(m.iter().map(|&i| &photos[i]));
        for &i in &m {
            places.insert(photos[i].id, StackPlace { stack, cover: photos[i].id == cover });
        }
    }
    places
}

/// The photo to show for a stack.
pub fn cover<'a>(members: impl Iterator<Item = &'a StackCandidate>) -> i64 {
    members
        .max_by_key(|p| (p.rating.unwrap_or(0), p.pixels, !p.raw, p.mtime_ns, std::cmp::Reverse(p.id)))
        .map(|p| p.id)
        .expect("a stack has members")
}

/// Regroups the whole library. Returns the photos whose stack changed.
pub fn restack(catalog: &mut Catalog) -> Result<Vec<i64>, tintype_catalog::Error> {
    let photos = catalog.stack_candidates(None)?;
    let embeddings: HashMap<i64, Vec<f32>> = catalog.embeddings()?.into_iter().collect();
    let places = group(&photos, &embeddings);
    catalog.set_stacks(&places, None)
}

/// Picks `photo`'s stack cover again (after a rating changed). Returns the
/// photos whose place changed.
pub fn refresh_cover(catalog: &mut Catalog, photo: i64) -> Result<Vec<i64>, tintype_catalog::Error> {
    let Some(stack) = catalog.stack_of(photo)? else { return Ok(Vec::new()) };
    let members = catalog.stack_candidates(Some(stack))?;
    let cover = cover(members.iter());
    let ids: Vec<i64> = members.iter().map(|m| m.id).collect();
    let places = ids.iter().map(|&id| (id, StackPlace { stack, cover: id == cover })).collect();
    catalog.set_stacks(&places, Some(&ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(id: i64, camera: &str, taken: Option<i64>) -> StackCandidate {
        StackCandidate {
            id,
            hash: Some(format!("h{id}")),
            camera: Some(camera.into()),
            serial: None,
            taken,
            rating: None,
            pixels: 0,
            raw: false,
            mtime_ns: 0,
        }
    }

    /// A unit vector at `angle` radians in a plane, so similarities are cosines.
    fn emb(angle: f32) -> Vec<f32> {
        vec![angle.cos(), angle.sin()]
    }

    fn stacks(places: &HashMap<i64, StackPlace>) -> Vec<Vec<i64>> {
        let mut by: HashMap<i64, Vec<i64>> = HashMap::new();
        for (id, p) in places {
            by.entry(p.stack).or_default().push(*id);
        }
        let mut out: Vec<Vec<i64>> = by
            .into_values()
            .map(|mut v| {
                v.sort();
                v
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn same_shot_and_exact_copies() {
        let mut raw = photo(1, "A", Some(100));
        raw.raw = true;
        let mut copy = photo(4, "B", Some(500));
        copy.hash = Some("h3".into());
        let photos = [raw, photo(2, "A", Some(100)), photo(3, "B", Some(100)), copy];
        let places = group(&photos, &HashMap::new());
        assert_eq!(stacks(&places), vec![vec![1, 2], vec![3, 4]], "other camera, same second: not the same shot");
        assert!(places[&2].cover && !places[&1].cover, "the JPG covers its RAF");
    }

    #[test]
    fn exports_without_serial_join_their_raw_but_other_bodies_do_not() {
        let mut raw = photo(1, "x-m5", Some(100));
        raw.serial = Some("2B000002".into());
        let export = photo(2, "x-m5", Some(100));
        let mut other_body = photo(3, "x-m5", Some(100));
        other_body.serial = Some("9Z999999".into());
        assert_eq!(stacks(&group(&[raw, export, other_body], &HashMap::new())), vec![vec![1, 2]]);
    }

    #[test]
    fn bursts_need_time_looks_and_a_short_span() {
        // 1 and 2: 5 s apart but reframed (cos 0.4 = 0.92), and unlike the rest. 3..=10: alike, 10 s
        // apart each, but a stack may span only 60 s, so 10 (70 s after 3) is left out.
        let mut photos = vec![photo(1, "A", Some(0)), photo(2, "A", Some(5))];
        photos.extend((3..=10).map(|id| photo(id, "A", Some(30 + 10 * (id - 3)))));
        let mut e: HashMap<i64, Vec<f32>> = (3..=10).map(|id| (id, emb(0.1 * (id % 2) as f32))).collect();
        e.insert(1, emb(1.0));
        e.insert(2, emb(1.4));
        assert_eq!(stacks(&group(&photos, &e)), vec![vec![3, 4, 5, 6, 7, 8, 9]]);

        // Another camera at the same moments never joins.
        photos.push(photo(11, "B", Some(30)));
        e.insert(11, emb(0.0));
        assert_eq!(stacks(&group(&photos, &e)), vec![vec![3, 4, 5, 6, 7, 8, 9]]);
    }

    #[test]
    fn repeats_up_to_a_minute_apart_need_near_identical_looks() {
        let photos = [photo(1, "A", Some(0)), photo(2, "A", Some(17)), photo(3, "A", Some(40))];
        // 1-2, 17 s apart: cos 0.2 = 0.980, a repeat. 2-3: cos 0.25 = 0.969, just short.
        let e: HashMap<i64, Vec<f32>> = [(1, emb(0.0)), (2, emb(0.2)), (3, emb(0.45))].into();
        assert_eq!(stacks(&group(&photos, &e)), vec![vec![1, 2]]);
    }

    #[test]
    fn undated_photos_stack_only_when_nearly_identical() {
        let photos = [photo(1, "A", Some(0)), photo(2, "A", None), photo(3, "A", None)];
        let e: HashMap<i64, Vec<f32>> = [(1, emb(0.0)), (2, emb(0.1)), (3, emb(0.5))].into();
        assert_eq!(stacks(&group(&photos, &e)), vec![vec![1, 2]]);
    }

    #[test]
    fn different_times_never_stack_by_looks() {
        let photos = [photo(1, "A", Some(0)), photo(2, "A", Some(40_000))];
        let e: HashMap<i64, Vec<f32>> = [(1, emb(0.0)), (2, emb(0.0))].into();
        assert!(group(&photos, &e).is_empty(), "sunrise and sunset from the same balcony");
    }

    #[test]
    fn cover_prefers_rating_then_size() {
        let mut a = photo(1, "A", None);
        let mut b = photo(2, "A", None);
        a.pixels = 10;
        b.pixels = 20;
        assert_eq!(cover([&a, &b].into_iter()), 2);
        a.rating = Some(3);
        assert_eq!(cover([&a, &b].into_iter()), 1);
    }
}
