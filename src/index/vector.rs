use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Router {
    pub dimension: u32,
    pub centroids: Vec<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuantizedVector {
    pub doc_idx: u32,
    pub scale: f32,
    pub values: Vec<i8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IvfList { pub vectors: Vec<QuantizedVector> }

pub fn normalize(v: &[f32]) -> Result<Vec<f32>> {
    if v.is_empty() { bail!("zero vector dimension") }
    if v.iter().any(|x| !x.is_finite()) { bail!("vector contains non-finite value") }
    let norm_sq = v.iter().map(|x| x * x).sum::<f32>();
    if !norm_sq.is_finite() || norm_sq <= 1e-24 { bail!("zero or invalid vector norm") }
    let norm = norm_sq.sqrt();
    Ok(v.iter().map(|x| x / norm).collect())
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() }

pub fn train_ivf(vectors: &[Vec<f32>], requested_lists: usize, iterations: usize) -> Result<(Router, Vec<IvfList>)> {
    train_ivf_iter(vectors.iter().map(Vec::as_slice), requested_lists, iterations)
}

pub fn train_ivf_iter<'a>(
    vectors: impl IntoIterator<Item = &'a [f32]>,
    requested_lists: usize,
    iterations: usize,
) -> Result<(Router, Vec<IvfList>)> {
    let vectors: Vec<&[f32]> = vectors.into_iter().collect();
    if vectors.is_empty() { bail!("cannot build IVF from zero vectors") }
    let dim = vectors.first().context("cannot build IVF from zero vectors")?.len();
    if dim == 0 || vectors.iter().any(|v| v.len() != dim) { bail!("inconsistent vector dimensions") }
    let normalized: Vec<Vec<f32>> = vectors.into_iter().map(normalize).collect::<Result<_>>()?;
    let k = requested_lists.max(1).min(normalized.len());

    // Deterministic farthest-point initialization with explicit selected-index
    // tracking. The old implementation could select the same vector repeatedly
    // when distances tied (e.g. identical vectors), creating duplicate centroids.
    let mut selected = BTreeSet::new();
    selected.insert(0usize);
    let first = normalized.first().context("normalized IVF corpus is empty")?.clone();
    let mut centroids = vec![first];
    while centroids.len() < k {
        let next = normalized.iter().enumerate()
            .filter(|(i, _)| !selected.contains(i))
            .map(|(i, v)| {
                let best_similarity = centroids.iter().map(|c| dot(v, c)).fold(-1.0f32, f32::max);
                (i, 1.0 - best_similarity)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .map(|x| x.0)
            .ok_or_else(|| anyhow::anyhow!("failed to choose distinct IVF centroid"))?;
        selected.insert(next);
        centroids.push(normalized[next].clone());
    }

    let mut assignment = vec![0usize; normalized.len()];
    for _ in 0..iterations.max(1) {
        for (index, vector) in normalized.iter().enumerate() {
            assignment[index] = nearest(vector, &centroids).context("IVF has no centroid")?;
        }
        let mut sums = vec![vec![0.0f32; dim]; k];
        let mut counts = vec![0usize; k];
        for (v, &a) in normalized.iter().zip(&assignment) {
            counts[a] += 1;
            for j in 0..dim { sums[a][j] += v[j]; }
        }
        for i in 0..k {
            if counts[i] > 0 { centroids[i] = normalize(&sums[i])?; }
        }
    }

    // Recompute assignment against final centroids. Without this step the old
    // code stored the assignment from before the last centroid update.
    for (index, vector) in normalized.iter().enumerate() {
        assignment[index] = nearest(vector, &centroids).context("IVF has no centroid")?;
    }

    let mut lists = vec![IvfList { vectors: Vec::new() }; k];
    for (doc_idx, (v, &a)) in normalized.iter().zip(&assignment).enumerate() {
        lists[a].vectors.push(quantize(u32::try_from(doc_idx).map_err(|_| anyhow::anyhow!("doc index overflow"))?, v));
    }
    let dimension = u32::try_from(dim).context("vector dimension exceeds u32")?;
    Ok((Router { dimension, centroids }, lists))
}

#[must_use]
pub fn nearest(v: &[f32], centroids: &[Vec<f32>]) -> Option<usize> {
    centroids
        .iter()
        .enumerate()
        .max_by(|(left_index, left), (right_index, right)| {
            dot(v, left)
                .total_cmp(&dot(v, right))
                .then_with(|| right_index.cmp(left_index))
        })
        .map(|(index, _)| index)
}

pub fn top_centroids(v: &[f32], centroids: &[Vec<f32>], nprobe: usize) -> Vec<usize> {
    let mut scored: Vec<_> = centroids.iter().enumerate().map(|(i, c)| (i, dot(v, c))).collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().take(nprobe.max(1)).map(|x| x.0).collect()
}

fn quantize(doc_idx: u32, v: &[f32]) -> QuantizedVector {
    let max_abs = v.iter().map(|x| x.abs()).fold(0.0f32, f32::max).max(1e-8);
    let scale = max_abs / 127.0;
    let values = v.iter().map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8).collect();
    QuantizedVector { doc_idx, scale, values }
}

pub fn approx_dot(query: &[f32], qv: &QuantizedVector) -> f32 {
    query.iter().zip(&qv.values).map(|(q, x)| *q * (*x as f32 * qv.scale)).sum()
}
