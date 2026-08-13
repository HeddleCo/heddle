// SPDX-License-Identifier: Apache-2.0
//! Accuracy benchmark for the semantic thread-reconstruction slice.

use std::{env, error::Error, path::PathBuf};

use heddle_semantic_recovery::{
    BgeSmallEmbedder, RecoveryIndex, ResidualQuantizerConfig, embed_documents,
};

#[path = "recovery/fixture.rs"]
mod fixture;
#[path = "recovery/metrics.rs"]
mod metrics;

use fixture::{DIVERGENCE_COUNT, SCENARIO_COUNT, fixture};
use metrics::{
    exact_hits, exact_sibling_hits, percent, print_breakdown, quantized_hits,
    quantized_sibling_hits,
};

const RECOVERY_FLOOR: f64 = 0.95;

fn main() -> Result<(), Box<dyn Error>> {
    let model_dir = env::var_os("HEDDLE_BGE_SMALL_MODEL_DIR")
        .map(PathBuf::from)
        .ok_or("set HEDDLE_BGE_SMALL_MODEL_DIR to the pinned local model directory")?;
    let (documents, classes) = fixture();
    let mut embedder = BgeSmallEmbedder::from_model_dir(model_dir)?;
    let (model, vectors) = embed_documents(&mut embedder, &documents)?;

    let oracle = exact_hits(&documents, &vectors);
    let oracle_siblings = exact_sibling_hits(&documents, &vectors);
    let sibling_total = documents.len() * (DIVERGENCE_COUNT - 1);
    println!(
        "semantic recovery benchmark: {} threads x {} states",
        SCENARIO_COUNT, DIVERGENCE_COUNT
    );
    println!(
        "full-float oracle: {}/{} = {:.2}%",
        oracle,
        documents.len(),
        percent(oracle, documents.len())
    );
    println!(
        "full-float sibling recall@6: {}/{} = {:.2}%",
        oracle_siblings,
        sibling_total,
        percent(oracle_siblings, sibling_total)
    );
    println!(
        "config\ttheoretical bits/vector\tpacked bits/vector\tthread hits\tthread hit-rate\tsibling recall@6\toracle gap"
    );

    let mut default_index = None;
    for config in configurations() {
        let (index, report) = RecoveryIndex::build_from_embeddings(
            &documents,
            model.clone(),
            vectors.clone(),
            config,
        )?;
        let hits = quantized_hits(&index, &documents, &vectors);
        let sibling_hits = quantized_sibling_hits(&index, &documents, &vectors);
        println!(
            "{}+{}\t{:.2}\t{}\t{}/{}\t{:.2}%\t{:.2}%\t{:.2} pp",
            config.coarse_centroids,
            config.residual_centroids,
            report.theoretical_bits_per_vector,
            report.packed_bits_per_vector,
            hits,
            documents.len(),
            percent(hits, documents.len()),
            percent(sibling_hits, sibling_total),
            percent(oracle_siblings, sibling_total) - percent(sibling_hits, sibling_total),
        );
        if config == ResidualQuantizerConfig::default() {
            default_index = Some(index);
        }
    }

    let index = default_index.ok_or("default 32+16 result missing")?;
    print_breakdown(&index, &documents, &vectors, &classes);
    let default_hits = quantized_hits(&index, &documents, &vectors);
    if default_hits as f64 / (documents.len() as f64) < RECOVERY_FLOOR {
        return Err(format!(
            "32+16 recovery {:.2}% is below the {:.0}% floor",
            percent(default_hits, documents.len()),
            RECOVERY_FLOOR * 100.0
        )
        .into());
    }
    Ok(())
}

fn configurations() -> [ResidualQuantizerConfig; 3] {
    [
        ResidualQuantizerConfig {
            coarse_centroids: 48,
            residual_centroids: 8,
            iterations: 20,
        },
        ResidualQuantizerConfig::default(),
        ResidualQuantizerConfig {
            coarse_centroids: 32,
            residual_centroids: 32,
            iterations: 20,
        },
    ]
}
