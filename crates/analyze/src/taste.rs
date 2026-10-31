//! Learning the user's taste: ridge regression from a photo's features (its
//! SigLIP embedding, plus the aesthetic score when there is one) to the
//! rating the user gave. Only ratings made in Tintype count; ratings imported
//! from sidecars may be another program's guesses.

/// Ratings needed before predictions are offered...
pub const MIN_RATINGS: usize = 30;
/// ...and they must not all be the same.
const MIN_DISTINCT: usize = 2;
/// Regularization. Embeddings are unit vectors and ratings 1-5, so this keeps
/// predictions near the user's average until many ratings agree.
const LAMBDA: f64 = 1.0;

/// A fitted model. Predictions are on the rating scale (1-5).
#[derive(Debug, Clone)]
pub struct TasteModel {
    weights: Vec<f64>,
    mean: f64,
    /// Ratings it was fitted on.
    pub trained_on: usize,
}

impl TasteModel {
    /// Fits on (features, rating) pairs, or `None` with too few or too uniform ratings.
    pub fn fit(samples: &[(Vec<f32>, f32)]) -> Option<Self> {
        let mut distinct: Vec<i32> = samples.iter().map(|(_, r)| r.round() as i32).collect();
        distinct.sort_unstable();
        distinct.dedup();
        if samples.len() < MIN_RATINGS || distinct.len() < MIN_DISTINCT {
            return None;
        }
        let d = samples[0].0.len();
        if samples.iter().any(|(f, _)| f.len() != d) {
            return None;
        }
        let mean = samples.iter().map(|(_, r)| f64::from(*r)).sum::<f64>() / samples.len() as f64;
        // Solve (X'X + λI) w = X'(y - mean).
        let mut a = vec![0f64; d * d];
        let mut b = vec![0f64; d];
        for (f, r) in samples {
            let y = f64::from(*r) - mean;
            for i in 0..d {
                let fi = f64::from(f[i]);
                b[i] += fi * y;
                let row = &mut a[i * d..i * d + d];
                for (j, fj) in f.iter().enumerate().skip(i) {
                    row[j] += fi * f64::from(*fj);
                }
            }
        }
        for i in 0..d {
            for j in 0..i {
                a[i * d + j] = a[j * d + i];
            }
            a[i * d + i] += LAMBDA;
        }
        let weights = cholesky_solve(a, b, d)?;
        Some(Self { weights, mean, trained_on: samples.len() })
    }

    /// The predicted rating, clamped to 1-5.
    pub fn predict(&self, features: &[f32]) -> f32 {
        let y = self.mean + self.weights.iter().zip(features).map(|(w, f)| w * f64::from(*f)).sum::<f64>();
        y.clamp(1.0, 5.0) as f32
    }
}

/// The features of a photo: its embedding, and its aesthetic score centred
/// on a typical value (scores mostly fall between 4 and 7).
pub fn features(embedding: &[f32], aesthetic: Option<f32>) -> Vec<f32> {
    let mut f = embedding.to_vec();
    f.push(aesthetic.map_or(0.0, |a| (a - 5.3) / 1.0));
    f
}

/// Solves `a x = b` for symmetric positive definite `a` (n x n, row-major).
fn cholesky_solve(mut a: Vec<f64>, mut b: Vec<f64>, n: usize) -> Option<Vec<f64>> {
    // In place: the lower triangle becomes L with a = L L'.
    for j in 0..n {
        let mut s = a[j * n + j];
        for k in 0..j {
            s -= a[j * n + k] * a[j * n + k];
        }
        if s <= 0.0 {
            return None;
        }
        let ljj = s.sqrt();
        a[j * n + j] = ljj;
        for i in j + 1..n {
            let mut s = a[i * n + j];
            for k in 0..j {
                s -= a[i * n + k] * a[j * n + k];
            }
            a[i * n + j] = s / ljj;
        }
    }
    // L z = b, then L' x = z.
    for i in 0..n {
        let mut s = b[i];
        for k in 0..i {
            s -= a[i * n + k] * b[k];
        }
        b[i] = s / a[i * n + i];
    }
    for i in (0..n).rev() {
        let mut s = b[i];
        for k in i + 1..n {
            s -= a[k * n + i] * b[k];
        }
        b[i] = s / a[i * n + i];
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cholesky_solves_a_small_system() {
        // [[4, 2], [2, 3]] x = [2, 1] -> x = [0.5, 0].
        let x = cholesky_solve(vec![4.0, 2.0, 2.0, 3.0], vec![2.0, 1.0], 2).unwrap();
        assert!((x[0] - 0.5).abs() < 1e-12 && x[1].abs() < 1e-12);
    }

    #[test]
    fn learns_a_preference_and_needs_enough_ratings() {
        // The user loves photos pointing along axis 0 and dislikes axis 1.
        let unit = |a: f32| vec![a.cos(), a.sin(), 0.0];
        let samples: Vec<(Vec<f32>, f32)> = (0..40)
            .map(|i| {
                let a = i as f32 / 40.0 * std::f32::consts::FRAC_PI_2;
                (unit(a), 5.0 - 4.0 * a / std::f32::consts::FRAC_PI_2)
            })
            .collect();
        assert!(TasteModel::fit(&samples[..MIN_RATINGS - 1]).is_none(), "too few");
        let uniform: Vec<(Vec<f32>, f32)> = samples.iter().map(|(f, _)| (f.clone(), 4.0)).collect();
        assert!(TasteModel::fit(&uniform).is_none(), "all the same rating says nothing");

        let m = TasteModel::fit(&samples).unwrap();
        let (loved, disliked) = (m.predict(&unit(0.05)), m.predict(&unit(1.5)));
        assert!(loved > 4.0 && disliked < 2.5, "{loved} {disliked}");
        assert_eq!(m.trained_on, 40);
    }

    #[test]
    fn features_append_the_aesthetic_score() {
        assert_eq!(features(&[1.0, 0.0], Some(6.3)), vec![1.0, 0.0, 1.0]);
        assert_eq!(features(&[1.0, 0.0], None)[2], 0.0);
    }
}
