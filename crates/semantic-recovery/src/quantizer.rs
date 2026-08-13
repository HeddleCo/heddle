// SPDX-License-Identifier: Apache-2.0
//! Deterministic two-stage residual quantization.

use serde::{Deserialize, Serialize};

use crate::{RecoveryError, Result, document::normalize};

/// Residual codebook sizes and deterministic Lloyd iteration count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResidualQuantizerConfig {
    /// First-stage centroid count.
    pub coarse_centroids: usize,
    /// Residual-stage centroid count.
    pub residual_centroids: usize,
    /// Maximum Lloyd iterations per stage.
    pub iterations: usize,
}

impl Default for ResidualQuantizerConfig {
    fn default() -> Self {
        Self {
            coarse_centroids: 32,
            residual_centroids: 16,
            iterations: 20,
        }
    }
}

/// Ideal information content of the two codebook assignments.
pub fn theoretical_bits_per_vector(config: ResidualQuantizerConfig) -> f64 {
    (config.coarse_centroids as f64).log2() + (config.residual_centroids as f64).log2()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VectorCode {
    pub coarse: usize,
    pub residual: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ResidualQuantizer {
    pub config: ResidualQuantizerConfig,
    pub dimensions: usize,
    pub coarse: Vec<Vec<f32>>,
    pub residual: Vec<Vec<f32>>,
}

impl ResidualQuantizer {
    pub fn train(vectors: &[Vec<f32>], config: ResidualQuantizerConfig) -> Result<Self> {
        validate_config(config)?;
        let dimensions = validate_vectors(vectors)?;
        let coarse = train_codebook(vectors, config.coarse_centroids, config.iterations);
        let residual_vectors: Vec<Vec<f32>> = vectors
            .iter()
            .map(|vector| {
                let centroid = &coarse[nearest(vector, &coarse)];
                subtract(vector, centroid)
            })
            .collect();
        let residual = train_codebook(
            &residual_vectors,
            config.residual_centroids,
            config.iterations,
        );
        Ok(Self {
            config,
            dimensions,
            coarse,
            residual,
        })
    }

    pub fn encode(&self, vector: &[f32]) -> Result<VectorCode> {
        if vector.len() != self.dimensions || vector.iter().any(|value| !value.is_finite()) {
            return Err(RecoveryError::InvalidInput(format!(
                "expected {} finite vector dimensions",
                self.dimensions
            )));
        }
        let coarse = nearest(vector, &self.coarse);
        let remainder = subtract(vector, &self.coarse[coarse]);
        let residual = nearest(&remainder, &self.residual);
        Ok(VectorCode { coarse, residual })
    }

    pub fn decode(&self, code: VectorCode) -> Result<Vec<f32>> {
        let coarse = self.coarse.get(code.coarse).ok_or_else(|| {
            RecoveryError::InvalidSidecar("coarse code is outside its codebook".to_string())
        })?;
        let residual = self.residual.get(code.residual).ok_or_else(|| {
            RecoveryError::InvalidSidecar("residual code is outside its codebook".to_string())
        })?;
        let mut vector: Vec<f32> = coarse
            .iter()
            .zip(residual)
            .map(|(left, right)| left + right)
            .collect();
        normalize(&mut vector)?;
        Ok(vector)
    }

    pub fn packed_bits(&self) -> usize {
        code_bits(self.config.coarse_centroids) + code_bits(self.config.residual_centroids)
    }

    pub fn validate(&self) -> Result<()> {
        validate_config(self.config)?;
        validate_codebook(&self.coarse, self.config.coarse_centroids, self.dimensions)?;
        validate_codebook(
            &self.residual,
            self.config.residual_centroids,
            self.dimensions,
        )
    }
}

pub(crate) fn code_bits(cardinality: usize) -> usize {
    usize::BITS as usize - (cardinality - 1).leading_zeros() as usize
}

fn validate_config(config: ResidualQuantizerConfig) -> Result<()> {
    if !(2..=256).contains(&config.coarse_centroids)
        || !(2..=256).contains(&config.residual_centroids)
        || config.iterations == 0
    {
        return Err(RecoveryError::InvalidInput(
            "codebook sizes must be 2..=256 and iterations must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_vectors(vectors: &[Vec<f32>]) -> Result<usize> {
    let dimensions = vectors.first().map(Vec::len).unwrap_or_default();
    if dimensions == 0
        || vectors
            .iter()
            .any(|vector| vector.len() != dimensions || vector.iter().any(|v| !v.is_finite()))
    {
        return Err(RecoveryError::InvalidInput(
            "vectors must be non-empty, finite, and dimensionally consistent".to_string(),
        ));
    }
    Ok(dimensions)
}

fn validate_codebook(codebook: &[Vec<f32>], count: usize, dimensions: usize) -> Result<()> {
    if codebook.len() != count
        || codebook
            .iter()
            .any(|vector| vector.len() != dimensions || vector.iter().any(|v| !v.is_finite()))
    {
        return Err(RecoveryError::InvalidSidecar(
            "codebook shape or values are invalid".to_string(),
        ));
    }
    Ok(())
}

fn train_codebook(vectors: &[Vec<f32>], count: usize, iterations: usize) -> Vec<Vec<f32>> {
    let mut centroids = farthest_first(vectors, count);
    for _ in 0..iterations {
        let dimensions = vectors[0].len();
        let mut totals = vec![vec![0.0_f32; dimensions]; count];
        let mut counts = vec![0_usize; count];
        for vector in vectors {
            let cluster = nearest(vector, &centroids);
            counts[cluster] += 1;
            for (total, value) in totals[cluster].iter_mut().zip(vector) {
                *total += value;
            }
        }
        let mut changed = false;
        for index in 0..count {
            if counts[index] == 0 {
                continue;
            }
            totals[index]
                .iter_mut()
                .for_each(|value| *value /= counts[index] as f32);
            changed |= squared_distance(&centroids[index], &totals[index]) > 1e-12;
            centroids[index] = std::mem::take(&mut totals[index]);
        }
        if !changed {
            break;
        }
    }
    centroids
}

fn farthest_first(vectors: &[Vec<f32>], count: usize) -> Vec<Vec<f32>> {
    let mut centroids = vec![vectors[0].clone()];
    while centroids.len() < count {
        let next = vectors
            .iter()
            .enumerate()
            .max_by(|(left_index, left), (right_index, right)| {
                let left_distance = nearest_distance(left, &centroids);
                let right_distance = nearest_distance(right, &centroids);
                left_distance
                    .total_cmp(&right_distance)
                    .then_with(|| right_index.cmp(left_index))
            })
            .map(|(_, vector)| vector.clone())
            .unwrap_or_else(|| vectors[centroids.len() % vectors.len()].clone());
        centroids.push(next);
    }
    centroids
}

fn nearest(vector: &[f32], centroids: &[Vec<f32>]) -> usize {
    centroids
        .iter()
        .enumerate()
        .min_by(|(left_index, left), (right_index, right)| {
            squared_distance(vector, left)
                .total_cmp(&squared_distance(vector, right))
                .then_with(|| left_index.cmp(right_index))
        })
        .map(|(index, _)| index)
        .unwrap_or_default()
}

fn nearest_distance(vector: &[f32], centroids: &[Vec<f32>]) -> f32 {
    squared_distance(vector, &centroids[nearest(vector, centroids)])
}

fn squared_distance(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(a, b)| {
            let difference = a - b;
            difference * difference
        })
        .sum()
}

fn subtract(left: &[f32], right: &[f32]) -> Vec<f32> {
    left.iter().zip(right).map(|(a, b)| a - b).collect()
}
