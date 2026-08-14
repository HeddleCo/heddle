// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use heddle_semantic_recovery::{RecoveryIndex, StateDocument};

use super::fixture::{DIVERGENCE_COUNT, Divergence};

pub fn exact_hits(documents: &[StateDocument], vectors: &[Vec<f32>]) -> usize {
    vectors
        .iter()
        .enumerate()
        .filter(|(query_index, query)| {
            let neighbor = vectors
                .iter()
                .enumerate()
                .filter(|(index, _)| index != query_index)
                .max_by(|(left_index, left), (right_index, right)| {
                    dot(query, left)
                        .total_cmp(&dot(query, right))
                        .then_with(|| right_index.cmp(left_index))
                })
                .map(|(index, _)| index)
                .expect("fixture has siblings");
            documents[neighbor].thread == documents[*query_index].thread
        })
        .count()
}

pub fn quantized_hits(
    index: &RecoveryIndex,
    documents: &[StateDocument],
    vectors: &[Vec<f32>],
) -> usize {
    documents
        .iter()
        .zip(vectors)
        .filter(|(document, vector)| {
            index
                .reconstruct_thread(document.state, vector, 4)
                .expect("valid query")
                .is_some_and(|result| result.thread == document.thread)
        })
        .count()
}

pub fn exact_sibling_hits(documents: &[StateDocument], vectors: &[Vec<f32>]) -> usize {
    vectors
        .iter()
        .enumerate()
        .map(|(query_index, query)| {
            let mut neighbors: Vec<_> = vectors
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != query_index)
                .map(|(index, vector)| (index, dot(query, vector)))
                .collect();
            neighbors.sort_by(|left, right| {
                right
                    .1
                    .total_cmp(&left.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            neighbors
                .into_iter()
                .take(DIVERGENCE_COUNT - 1)
                .filter(|(index, _)| documents[*index].thread == documents[query_index].thread)
                .count()
        })
        .sum()
}

pub fn quantized_sibling_hits(
    index: &RecoveryIndex,
    documents: &[StateDocument],
    vectors: &[Vec<f32>],
) -> usize {
    documents
        .iter()
        .zip(vectors)
        .map(|(document, vector)| {
            index
                .search(vector, DIVERGENCE_COUNT - 1, Some(document.state))
                .expect("valid query")
                .into_iter()
                .filter(|neighbor| neighbor.thread == document.thread)
                .count()
        })
        .sum()
}

pub fn print_breakdown(
    index: &RecoveryIndex,
    documents: &[StateDocument],
    vectors: &[Vec<f32>],
    classes: &[Divergence],
) {
    let mut totals: BTreeMap<Divergence, (usize, usize)> = BTreeMap::new();
    for ((document, vector), class) in documents.iter().zip(vectors).zip(classes) {
        let hit = index
            .reconstruct_thread(document.state, vector, 4)
            .expect("valid query")
            .is_some_and(|result| result.thread == document.thread);
        let entry = totals.entry(*class).or_default();
        entry.0 += usize::from(hit);
        entry.1 += 1;
    }
    println!("\n32+16 recovery by divergence class:");
    for class in Divergence::ALL {
        let (hits, total) = totals[&class];
        println!(
            "{}\t{hits}/{total}\t{:.2}%",
            class.name(),
            percent(hits, total)
        );
    }
}

pub fn percent(hits: usize, total: usize) -> f64 {
    100.0 * hits as f64 / total as f64
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}
