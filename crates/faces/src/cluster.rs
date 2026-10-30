//! Grouping and matching face embeddings by cosine similarity.
//!
//! Everything compares faces with faces, never with an average face: SFace
//! embeddings share a common "face-like" direction, so the mean of a large,
//! mixed set ends up similar to everyone and swallows unrelated people.

use std::collections::HashMap;

use rayon::prelude::*;

/// Scales `v` to unit length (no-op for the zero vector).
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

/// Cosine similarity of two unit vectors.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// How well `face` matches a set of known faces of one person: the mean of
/// its `k` best similarities. More robust than the best single match (one
/// odd photo) or the similarity to the set's mean.
pub fn match_score(face: &[f32], known: &[&[f32]], k: usize) -> f32 {
    let mut sims: Vec<f32> = known.iter().map(|e| similarity(face, e)).collect();
    if sims.is_empty() {
        return 0.0;
    }
    sims.sort_by(|a, b| b.total_cmp(a));
    let k = k.clamp(1, sims.len());
    sims[..k].iter().sum::<f32>() / k as f32
}

/// Groups faces with Chinese Whispers: faces are linked when their
/// similarity reaches `threshold`, then each repeatedly adopts the group
/// most strongly represented among its links. Returns a group label per
/// input, numbered from 0 by decreasing group size; a face with no links is
/// a group of one.
pub fn cluster(embeddings: &[&[f32]], threshold: f32) -> Vec<usize> {
    let n = embeddings.len();
    let links: Vec<Vec<(usize, f32)>> = (0..n)
        .into_par_iter()
        .map(|i| {
            (0..n)
                .filter(|&j| j != i)
                .filter_map(|j| {
                    let s = similarity(embeddings[i], embeddings[j]);
                    (s >= threshold).then_some((j, s))
                })
                .collect()
        })
        .collect();

    let mut labels: Vec<usize> = (0..n).collect();
    let mut order: Vec<usize> = (0..n).collect();
    // Fixed-seed xorshift: deterministic results for the same input.
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut weights: HashMap<usize, f32> = HashMap::new();
    for _ in 0..50 {
        for i in (1..n).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            order.swap(i, (rng % (i as u64 + 1)) as usize);
        }
        let mut changed = false;
        for &i in &order {
            if links[i].is_empty() {
                continue;
            }
            weights.clear();
            for &(j, w) in &links[i] {
                *weights.entry(labels[j]).or_default() += w;
            }
            let best = weights
                .iter()
                .max_by(|a, b| a.1.total_cmp(b.1).then(b.0.cmp(a.0)))
                .map(|(&l, _)| l)
                .expect("links are not empty");
            if best != labels[i] {
                labels[i] = best;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Renumber by size, largest first; ties by first appearance.
    let mut sizes: HashMap<usize, (usize, usize)> = HashMap::new();
    for (i, &l) in labels.iter().enumerate() {
        sizes.entry(l).or_insert((0, i)).0 += 1;
    }
    let mut order: Vec<usize> = sizes.keys().copied().collect();
    order.sort_by(|a, b| sizes[b].0.cmp(&sizes[a].0).then(sizes[a].1.cmp(&sizes[b].1)));
    let rank: HashMap<usize, usize> = order.iter().enumerate().map(|(i, &l)| (l, i)).collect();
    labels.iter().map(|l| rank[l]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let mut v = v.to_vec();
        normalize(&mut v);
        v
    }

    #[test]
    fn groups_similar_vectors() {
        let a1 = unit(&[1.0, 0.1, 0.0]);
        let a2 = unit(&[1.0, -0.1, 0.05]);
        let a3 = unit(&[0.95, 0.0, 0.1]);
        let b1 = unit(&[0.0, 1.0, 0.1]);
        let b2 = unit(&[0.1, 1.0, 0.0]);
        let c = unit(&[0.0, 0.0, 1.0]);
        let refs: Vec<&[f32]> = [&a1, &b1, &a2, &c, &b2, &a3].iter().map(|v| v.as_slice()).collect();
        let labels = cluster(&refs, 0.8);
        assert_eq!(labels, vec![0, 1, 0, 2, 1, 0]);
    }

    #[test]
    fn does_not_chain_through_a_shared_component() {
        // Every vector shares a strong common direction (like real face embeddings)
        // plus one of four identities. Averages would merge them all; pairs don't.
        let mut faces = Vec::new();
        for id in 0..4 {
            for k in 0..5 {
                let mut v = vec![0.0f32; 8];
                v[0] = 0.75; // common "face" direction
                v[1 + id] = 1.0;
                v[5 + (k % 3)] = 0.25; // per-photo noise
                faces.push(unit(&v));
            }
        }
        let refs: Vec<&[f32]> = faces.iter().map(|v| v.as_slice()).collect();
        let labels = cluster(&refs, 0.75);
        for id in 0..4 {
            let group = &labels[id * 5..id * 5 + 5];
            assert!(group.iter().all(|&l| l == group[0]), "identity {id} split: {labels:?}");
        }
        let mut distinct: Vec<usize> = labels.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 4, "{labels:?}");
    }

    #[test]
    fn match_score_uses_best_k() {
        let face = unit(&[1.0, 0.0]);
        let good = unit(&[1.0, 0.1]);
        let bad = unit(&[0.0, 1.0]);
        let known: Vec<&[f32]> = vec![&bad, &good, &good];
        let s2 = match_score(&face, &known, 2);
        assert!((s2 - similarity(&face, &good)).abs() < 1e-6);
        assert_eq!(match_score(&face, &[], 3), 0.0);
    }

    #[test]
    fn empty_input() {
        assert!(cluster(&[], 0.5).is_empty());
    }
}
