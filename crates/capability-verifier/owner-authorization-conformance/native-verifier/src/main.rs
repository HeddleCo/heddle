// SPDX-License-Identifier: MIT OR Apache-2.0

mod import;

use std::{env, fs, process::ExitCode};

use heddleco_capability_verifier::conformance::{
    run_fixture, run_keyring_fixture, run_timeline_fixture, run_transfer_fixture,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    fixture_kind: FixtureKind,
    fixture_json: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FixtureKind {
    Purge,
    Transfer,
    Keyring,
    Timeline,
    Import,
}

#[derive(Serialize)]
struct Outcome {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn main() -> ExitCode {
    match run()
        .and_then(|outcomes| serde_json::to_string(&outcomes).map_err(|error| error.to_string()))
    {
        Ok(serialized) => {
            if env::args().nth(1).as_deref() != Some("--import-corpus") {
                println!("{serialized}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<Vec<Outcome>, String> {
    if env::args().nth(1).as_deref() == Some("--import-corpus") {
        let path = env::args_os().nth(2).ok_or("missing fixture path")?;
        let fixture: Value = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        println!("{}", import::cases(&fixture)?);
        return Ok(Vec::new());
    }
    let path = env::args_os()
        .nth(1)
        .ok_or_else(|| "missing corpus path".to_owned())?;
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let corpus: Corpus = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    corpus
        .cases
        .into_iter()
        .map(|case| {
            let result = evaluate(case.fixture_kind, &case.fixture_json);
            match result {
                Ok(ok) => Ok(Outcome {
                    id: case.id,
                    ok: Some(ok),
                    error: None,
                }),
                Err(error) => Ok(Outcome {
                    id: case.id,
                    ok: None,
                    error: Some(error),
                }),
            }
        })
        .collect()
}

fn evaluate(kind: FixtureKind, fixture_json: &str) -> Result<Value, String> {
    match kind {
        FixtureKind::Import => return import::evaluate(fixture_json),
        FixtureKind::Purge => {
            serde_json::to_value(run_fixture(fixture_json).map_err(|error| error.to_string())?)
        }
        FixtureKind::Transfer => serde_json::to_value(
            run_transfer_fixture(fixture_json).map_err(|error| error.to_string())?,
        ),
        FixtureKind::Keyring => serde_json::to_value(
            run_keyring_fixture(fixture_json).map_err(|error| error.to_string())?,
        ),
        FixtureKind::Timeline => serde_json::to_value(
            run_timeline_fixture(fixture_json).map_err(|error| error.to_string())?,
        ),
    }
    .map_err(|error| error.to_string())
}
