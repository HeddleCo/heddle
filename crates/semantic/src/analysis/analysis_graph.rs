// SPDX-License-Identifier: Apache-2.0
//! Call graph and blast radius analysis.
//!
//! Extracts function call relationships from tree-sitter ASTs and computes
//! downstream impact ("blast radius") for changed functions.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
};

use crate::parser::{Language, ParsedFile};

/// A node in the call graph: a function definition.
#[derive(Clone, Debug)]
pub struct CallGraphNode {
    /// File containing this function.
    pub file: PathBuf,
    /// Function name.
    pub name: String,
    /// Line range in the source file.
    pub start_line: usize,
    pub end_line: usize,
}

/// A directed edge: `caller` calls `callee`.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct FunctionKey {
    pub file: PathBuf,
    pub name: String,
    pub start_line: usize,
}

/// A directed edge: `caller` calls `callee`.
#[derive(Clone, Debug)]
pub struct CallEdge {
    pub caller: FunctionKey,
    pub callee: FunctionKey,
}

/// The call graph for a set of files.
#[derive(Clone, Debug, Default)]
pub struct CallGraph {
    /// Function definitions: name → node.
    pub nodes: HashMap<FunctionKey, CallGraphNode>,
    /// Forward edges: function → set of functions it calls.
    pub calls: HashMap<FunctionKey, HashSet<FunctionKey>>,
    /// Reverse edges: function → set of functions that call it.
    pub called_by: HashMap<FunctionKey, HashSet<FunctionKey>>,
}

/// Blast radius result for a set of changed functions.
#[derive(Clone, Debug)]
pub struct BlastRadius {
    /// The changed functions that triggered the analysis.
    pub changed_functions: Vec<String>,
    /// Downstream functions affected (transitive callers).
    pub affected: Vec<CallGraphNode>,
    /// Total number of affected functions.
    pub affected_count: usize,
}

impl CallGraph {
    /// Build a call graph from a set of files and their contents.
    pub fn build(files: &[(PathBuf, String)]) -> Self {
        let mut graph = CallGraph::default();

        for (path, content) in files {
            let language = Language::from_path(path);
            let Some(parsed) = ParsedFile::parse(content, language) else {
                continue;
            };

            let functions: Vec<_> = parsed
                .extract_functions()
                .into_iter()
                .map(|func| {
                    let key = FunctionKey {
                        file: path.clone(),
                        name: func.name.clone(),
                        start_line: func.start_line,
                    };
                    (key, func)
                })
                .collect();

            for (key, func) in &functions {
                graph.nodes.insert(
                    key.clone(),
                    CallGraphNode {
                        file: path.clone(),
                        name: func.name.clone(),
                        start_line: func.start_line,
                        end_line: func.end_line,
                    },
                );
            }

            let edges = extract_call_edges(&functions, parsed.language);
            for edge in edges {
                graph
                    .calls
                    .entry(edge.caller.clone())
                    .or_default()
                    .insert(edge.callee.clone());
                graph
                    .called_by
                    .entry(edge.callee.clone())
                    .or_default()
                    .insert(edge.caller.clone());
            }
        }

        graph
    }

    /// Return stable function identities for a bare function name.
    pub fn keys_for_name(&self, name: &str) -> Vec<FunctionKey> {
        let mut keys: Vec<_> = self
            .nodes
            .keys()
            .filter(|key| key.name == name)
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Compute the blast radius for a set of changed function names.
    /// Returns all transitive callers (upstream functions that depend on the changed ones).
    pub fn blast_radius(&self, changed_functions: &[String]) -> BlastRadius {
        let mut affected: HashSet<FunctionKey> = HashSet::new();
        let mut queue: VecDeque<FunctionKey> = VecDeque::new();
        let changed_names: HashSet<_> = changed_functions.iter().map(String::as_str).collect();
        let changed_keys: HashSet<_> = self
            .nodes
            .keys()
            .filter(|key| changed_names.contains(key.name.as_str()))
            .cloned()
            .collect();

        for key in &changed_keys {
            queue.push_back(key.clone());
        }

        while let Some(current) = queue.pop_front() {
            if let Some(callers) = self.called_by.get(&current) {
                for caller in callers {
                    if !changed_keys.contains(caller) && affected.insert(caller.clone()) {
                        queue.push_back(caller.clone());
                    }
                }
            }
        }

        let mut affected_nodes: Vec<CallGraphNode> = affected
            .iter()
            .filter_map(|key| self.nodes.get(key).cloned())
            .collect();
        affected_nodes.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then_with(|| left.start_line.cmp(&right.start_line))
                .then_with(|| left.name.cmp(&right.name))
        });
        let count = affected_nodes.len();

        BlastRadius {
            changed_functions: changed_functions.to_vec(),
            affected: affected_nodes,
            affected_count: count,
        }
    }
}

/// Extract call edges from a parsed file by walking function bodies
/// and looking for identifier references that match known patterns.
fn extract_call_edges(
    functions: &[(FunctionKey, crate::parser::FunctionDef)],
    language: Language,
) -> Vec<CallEdge> {
    let mut edges = Vec::new();
    let mut functions_by_name: HashMap<String, Vec<FunctionKey>> = HashMap::new();

    for (key, func) in functions {
        functions_by_name
            .entry(func.name.clone())
            .or_default()
            .push(key.clone());
    }

    for (caller_key, func) in functions {
        let calls = extract_calls_from_text(&func.content, language);
        for callee_name in calls {
            if callee_name == func.name {
                continue;
            }

            if let Some(callees) = functions_by_name.get(&callee_name) {
                for callee_key in callees {
                    edges.push(CallEdge {
                        caller: caller_key.clone(),
                        callee: callee_key.clone(),
                    });
                }
            }
        }
    }

    edges
}

/// Extract function call names from a source text snippet.
/// Simple heuristic: look for `identifier(` patterns.
fn extract_calls_from_text(text: &str, _language: Language) -> Vec<String> {
    let mut calls = HashSet::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Find '(' and look backwards for an identifier.
        if bytes[i] == b'(' {
            let end = i;
            // Skip backwards past whitespace.
            let mut j = end;
            while j > 0 && bytes[j - 1] == b' ' {
                j -= 1;
            }
            // Collect identifier characters backwards.
            let ident_end = j;
            while j > 0 && (bytes[j - 1].is_ascii_alphanumeric() || bytes[j - 1] == b'_') {
                j -= 1;
            }
            if j < ident_end {
                let ident = &text[j..ident_end];
                // Filter out language keywords.
                if !is_keyword(ident) && !ident.is_empty() {
                    calls.insert(ident.to_string());
                }
            }
        }
        i += 1;
    }

    calls.into_iter().collect()
}

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "if" | "else"
            | "for"
            | "while"
            | "match"
            | "return"
            | "let"
            | "mut"
            | "fn"
            | "pub"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "use"
            | "mod"
            | "type"
            | "where"
            | "async"
            | "await"
            | "loop"
            | "break"
            | "continue"
            | "self"
            | "Self"
            | "super"
            | "crate"
            | "as"
            | "in"
            | "ref"
            | "move"
            | "dyn"
            | "unsafe"
            | "extern"
            | "const"
            | "static"
            | "true"
            | "false"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
            // Common control flow in other languages
            | "def"
            | "class"
            | "import"
            | "from"
            | "try"
            | "catch"
            | "throw"
            | "new"
            | "var"
            | "function"
            | "switch"
            | "case"
            | "default"
            | "typeof"
            | "instanceof"
            | "void"
            | "delete"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_call_graph() {
        let files = vec![(
            PathBuf::from("test.rs"),
            concat!(
                "fn main() {\n",
                "    let x = helper();\n",
                "    process(x);\n",
                "}\n\n",
                "fn helper() -> i32 {\n",
                "    42\n",
                "}\n\n",
                "fn process(x: i32) {\n",
                "    output(x);\n",
                "}\n\n",
                "fn output(x: i32) {\n",
                "    println!(\"{}\", x);\n",
                "}\n",
            )
            .to_string(),
        )];

        let graph = CallGraph::build(&files);
        assert_eq!(graph.nodes.len(), 4); // main, helper, process, output

        // main calls helper and process.
        let main_key = graph.keys_for_name("main").pop().unwrap();
        let helper_key = graph.keys_for_name("helper").pop().unwrap();
        let process_key = graph.keys_for_name("process").pop().unwrap();
        let output_key = graph.keys_for_name("output").pop().unwrap();
        let main_calls = graph.calls.get(&main_key).unwrap();
        assert!(main_calls.contains(&helper_key));
        assert!(main_calls.contains(&process_key));

        // process calls output.
        let proc_calls = graph.calls.get(&process_key).unwrap();
        assert!(proc_calls.contains(&output_key));
    }

    #[test]
    fn test_blast_radius() {
        let files = vec![(
            PathBuf::from("test.rs"),
            concat!(
                "fn main() {\n    run();\n}\n\n",
                "fn run() {\n    compute();\n}\n\n",
                "fn compute() {\n    42\n}\n\n",
                "fn unrelated() {\n    99\n}\n",
            )
            .to_string(),
        )];

        let graph = CallGraph::build(&files);
        let blast = graph.blast_radius(&["compute".to_string()]);

        // compute is called by run, run is called by main.
        assert_eq!(blast.affected_count, 2);
        let names: HashSet<_> = blast.affected.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains("run"));
        assert!(names.contains("main"));
        // unrelated is not affected.
        assert!(!names.contains("unrelated"));
    }

    #[test]
    fn test_no_blast_radius_for_leaf() {
        let files = vec![(
            PathBuf::from("test.rs"),
            "fn leaf() {\n    42\n}\n".to_string(),
        )];

        let graph = CallGraph::build(&files);
        let blast = graph.blast_radius(&["leaf".to_string()]);
        assert_eq!(blast.affected_count, 0);
    }

    #[test]
    fn test_duplicate_function_names_stay_isolated_by_file() {
        let files = vec![
            (
                PathBuf::from("a.rs"),
                concat!(
                    "fn run() {\n    target();\n}\n\n",
                    "fn target() {\n    42\n}\n",
                )
                .to_string(),
            ),
            (
                PathBuf::from("b.rs"),
                concat!(
                    "fn run() {\n    other();\n}\n\n",
                    "fn other() {\n    99\n}\n",
                )
                .to_string(),
            ),
        ];

        let graph = CallGraph::build(&files);
        let run_keys = graph.keys_for_name("run");
        assert_eq!(run_keys.len(), 2);

        let blast = graph.blast_radius(&["target".to_string()]);
        assert_eq!(blast.affected_count, 1);
        assert_eq!(blast.affected[0].file, PathBuf::from("a.rs"));
        assert_eq!(blast.affected[0].name, "run");
    }
}
