// SPDX-License-Identifier: Apache-2.0
//! Output formatting for compare command.

use super::compare_types::{CompareOutput, SemanticChangeEntry};

pub fn write_output(output: &CompareOutput, json: bool) -> Result<(), serde_json::Error> {
    if json {
        println!("{}", serde_json::to_string(output)?);
        return Ok(());
    }

    println!("Comparing {} -> {}", output.state_a, output.state_b);
    println!();

    if output.changes.is_empty() && output.summary.renamed == 0 {
        println!("No differences");
        return Ok(());
    }

    for change in &output.changes {
        let prefix = match change.kind.as_str() {
            "added" => "+",
            "deleted" => "-",
            "modified" => "M",
            _ => "?",
        };
        println!("{} {}", prefix, change.path);
    }

    if let Some(ref semantic) = output.semantic_changes {
        for change in semantic {
            if change.change_type == "file_renamed" {
                println!(
                    "R {} -> {}",
                    change.from_path.as_deref().unwrap_or("?"),
                    change.to_path.as_deref().unwrap_or("?")
                );
            }
        }
    }

    println!();
    println!(
        "{} file(s) changed: {} added, {} modified, {} deleted, {} renamed",
        output.summary.total,
        output.summary.added,
        output.summary.modified,
        output.summary.deleted,
        output.summary.renamed
    );

    if let Some(ref semantic) = output.semantic_changes {
        let func_changes: Vec<_> = semantic
            .iter()
            .filter(|c| c.change_type.starts_with("function_"))
            .collect();
        let dep_changes: Vec<_> = semantic
            .iter()
            .filter(|c| c.change_type.starts_with("dependency_"))
            .collect();

        if !func_changes.is_empty() {
            println!();
            println!("Function changes:");
            print_semantic_list(&func_changes);
        }

        if !dep_changes.is_empty() {
            println!();
            println!("Dependency changes:");
            print_semantic_list(&dep_changes);
        }
    }

    Ok(())
}

fn print_semantic_list(changes: &[&SemanticChangeEntry]) {
    for change in changes {
        println!("  {}", change.description);
    }
}
