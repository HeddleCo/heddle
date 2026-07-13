// SPDX-License-Identifier: Apache-2.0
use std::{collections::BTreeSet, fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};
use prost_reflect::{
    DescriptorPool, ExtensionDescriptor, FieldDescriptor, Kind, MessageDescriptor, Value,
};

const CONTRACT_EXTENSION: &str = "heddle.v1.rpc_contract";
const IDEMPOTENCY_KEY_EXTENSION: &str = "heddle.v1.idempotency_key";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum RpcEffect {
    ReadOnly,
    TransientWrite,
    DurableWrite,
}

impl RpcEffect {
    fn from_number(number: i32) -> Result<Self> {
        match number {
            1 => Ok(Self::ReadOnly),
            2 => Ok(Self::TransientWrite),
            3 => Ok(Self::DurableWrite),
            _ => bail!("unknown or unspecified RpcEffect value {number}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum RpcDeduplication {
    None,
    ClientOperationId,
}

impl RpcDeduplication {
    fn from_number(number: i32) -> Result<Self> {
        match number {
            1 => Ok(Self::None),
            2 => Ok(Self::ClientOperationId),
            _ => bail!("unknown or unspecified RpcDeduplication value {number}"),
        }
    }
}

#[derive(Debug)]
struct MarkedField {
    path: String,
}

pub(super) fn run(workspace_root: &Path) -> Result<()> {
    let proto_root = workspace_root.join("crates/grpc/proto");
    let entrypoint = proto_root.join("heddle/v1/service.proto");
    let descriptor_set = compile_descriptor_set(&proto_root, &entrypoint)?;
    let pool = DescriptorPool::decode(descriptor_set.as_slice())
        .context("decode canonical gRPC descriptor set")?;

    let contract_extension = pool
        .get_extension_by_name(CONTRACT_EXTENSION)
        .with_context(|| format!("descriptor is missing extension {CONTRACT_EXTENSION}"))?;
    let key_extension = pool
        .get_extension_by_name(IDEMPOTENCY_KEY_EXTENSION)
        .with_context(|| format!("descriptor is missing extension {IDEMPOTENCY_KEY_EXTENSION}"))?;

    let mut failures = Vec::new();
    let mut durable_without_dedup = Vec::new();
    let mut counts = std::collections::BTreeMap::new();
    let mut method_count = 0usize;

    for service in pool
        .services()
        .filter(|service| service.package_name() == "heddle.v1")
    {
        for method in service.methods() {
            method_count += 1;
            let name = method.full_name().to_string();
            let contract = match rpc_contract(&method.options(), &contract_extension) {
                Ok(contract) => contract,
                Err(problem) => {
                    failures.push(format!("{name}: {problem:#}"));
                    continue;
                }
            };

            let mut marked = Vec::new();
            let mut key_problems = Vec::new();
            let input = method.input();
            let input_name = input.name().to_string();
            collect_marked_fields(
                &input,
                &key_extension,
                &mut BTreeSet::new(),
                &input_name,
                None,
                &mut marked,
                &mut key_problems,
            );
            failures.extend(
                key_problems
                    .into_iter()
                    .map(|problem| format!("{name}: {problem}")),
            );

            match contract.deduplication {
                RpcDeduplication::None if !marked.is_empty() => failures.push(format!(
                    "{name}: deduplication NONE has marked idempotency field(s): {}",
                    marked
                        .iter()
                        .map(|field| field.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                RpcDeduplication::ClientOperationId if marked.len() != 1 => failures.push(
                    format!(
                        "{name}: deduplication CLIENT_OPERATION_ID requires exactly one reachable marked field; found {}{}",
                        marked.len(),
                        if marked.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " ({})",
                                marked
                                    .iter()
                                    .map(|field| field.path.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                    ),
                ),
                _ => {}
            }

            if contract.effect == RpcEffect::ReadOnly
                && contract.deduplication != RpcDeduplication::None
            {
                failures.push(format!(
                    "{name}: READ_ONLY methods must declare deduplication NONE"
                ));
            }

            if contract.effect == RpcEffect::DurableWrite
                && contract.deduplication == RpcDeduplication::None
            {
                durable_without_dedup.push(name.clone());
            }

            let transport = match (method.is_client_streaming(), method.is_server_streaming()) {
                (false, false) => "unary",
                (true, false) => "client-streaming",
                (false, true) => "server-streaming",
                (true, true) => "bidirectional-streaming",
            };
            *counts
                .entry((contract.effect, contract.deduplication, transport))
                .or_insert(0usize) += 1;
        }
    }

    if method_count == 0 {
        bail!("audit-grpc-contract found no heddle.v1 methods");
    }
    if !failures.is_empty() {
        eprintln!(
            "audit-grpc-contract: {} contract violation(s):",
            failures.len()
        );
        for failure in &failures {
            eprintln!("  {failure}");
        }
        bail!("audit-grpc-contract failed");
    }

    println!(
        "audit-grpc-contract: {method_count} methods carry explicit effect and deduplication contracts."
    );
    for ((effect, deduplication, transport), count) in counts {
        println!("  {effect:?} + {deduplication:?} + {transport}: {count}");
    }
    if durable_without_dedup.is_empty() {
        println!("  durable writes without retry deduplication: none");
    } else {
        println!(
            "  durable writes without retry deduplication (known contract limitations): {}",
            durable_without_dedup.len()
        );
        for method in durable_without_dedup {
            println!("    {method}");
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Contract {
    effect: RpcEffect,
    deduplication: RpcDeduplication,
}

fn rpc_contract(
    options: &prost_reflect::DynamicMessage,
    extension: &ExtensionDescriptor,
) -> Result<Contract> {
    if !options.has_extension(extension) {
        bail!("missing explicit rpc_contract option");
    }
    let value = options.get_extension(extension);
    let Value::Message(contract) = value.as_ref() else {
        bail!("rpc_contract option is not a message");
    };
    let effect = match contract.get_field_by_name("effect").as_deref() {
        Some(Value::EnumNumber(number)) => RpcEffect::from_number(*number)?,
        _ => bail!("rpc_contract.effect is missing or not an enum"),
    };
    let deduplication = match contract.get_field_by_name("deduplication").as_deref() {
        Some(Value::EnumNumber(number)) => RpcDeduplication::from_number(*number)?,
        _ => bail!("rpc_contract.deduplication is missing or not an enum"),
    };
    Ok(Contract {
        effect,
        deduplication,
    })
}

fn collect_marked_fields(
    message: &MessageDescriptor,
    extension: &ExtensionDescriptor,
    ancestors: &mut BTreeSet<String>,
    path: &str,
    non_singular_edge: Option<&str>,
    marked: &mut Vec<MarkedField>,
    problems: &mut Vec<String>,
) {
    if !ancestors.insert(message.full_name().to_string()) {
        return;
    }

    let fields = message.fields().collect::<Vec<_>>();
    let has_direct_key = fields.iter().any(|field| field_is_marked(field, extension));

    for field in fields {
        let field_path = format!("{path}.{}", field.name());
        if field_is_marked(&field, extension) {
            validate_marked_field(&field, &field_path, problems);
            if let Some(edge) = non_singular_edge {
                problems.push(format!(
                    "marked idempotency field {field_path} is reached through non-singular edge {edge}"
                ));
            }
            marked.push(MarkedField {
                path: field_path.clone(),
            });
        }

        if !has_direct_key && let Kind::Message(child) = field.kind() {
            let child_non_singular_edge = non_singular_edge
                .or_else(|| (field.is_list() || field.is_map()).then_some(field_path.as_str()));
            collect_marked_fields(
                &child,
                extension,
                ancestors,
                &field_path,
                child_non_singular_edge,
                marked,
                problems,
            );
        }
    }

    ancestors.remove(message.full_name());
}

fn field_is_marked(field: &FieldDescriptor, extension: &ExtensionDescriptor) -> bool {
    let options = field.options();
    options.has_extension(extension)
        && matches!(options.get_extension(extension).as_ref(), Value::Bool(true))
}

fn validate_marked_field(field: &FieldDescriptor, path: &str, problems: &mut Vec<String>) {
    if field.name() != "client_operation_id" {
        problems.push(format!(
            "marked idempotency field {path} must be named client_operation_id"
        ));
    }
    if field.is_list() || field.is_map() {
        problems.push(format!("marked idempotency field {path} must be singular"));
    }
    if !matches!(field.kind(), Kind::String) {
        problems.push(format!(
            "marked idempotency field {path} must have type string"
        ));
    }
}

fn compile_descriptor_set(proto_root: &Path, entrypoint: &Path) -> Result<Vec<u8>> {
    let temp = tempfile::tempdir().context("create descriptor audit temp directory")?;
    let output = temp.path().join("heddle-descriptor.bin");
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let status = Command::new(&protoc)
        .arg(format!("--proto_path={}", proto_root.display()))
        .arg("--include_imports")
        .arg(format!("--descriptor_set_out={}", output.display()))
        .arg(entrypoint)
        .status()
        .with_context(|| format!("run protoc at '{}'", protoc.display()))?;
    if !status.success() {
        bail!("protoc descriptor generation exited with status {status}");
    }
    fs::read(&output).with_context(|| format!("read '{}'", output.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_pool(body: &str) -> DescriptorPool {
        let temp = tempfile::tempdir().expect("create fixture directory");
        let fixture = temp.path().join("fixture.proto");
        fs::write(
            &fixture,
            format!(
                "syntax = \"proto3\";\npackage fixture;\nimport \"heddle/v1/options.proto\";\n{body}\n"
            ),
        )
        .expect("write fixture proto");
        let descriptor = temp.path().join("fixture.bin");
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");
        let proto_root = workspace_root.join("crates/grpc/proto");
        let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
        let status = Command::new(protoc)
            .arg(format!("--proto_path={}", temp.path().display()))
            .arg(format!("--proto_path={}", proto_root.display()))
            .arg("--include_imports")
            .arg(format!("--descriptor_set_out={}", descriptor.display()))
            .arg(&fixture)
            .status()
            .expect("run protoc");
        assert!(status.success(), "fixture protoc failed with {status}");
        DescriptorPool::decode(
            fs::read(descriptor)
                .expect("read fixture descriptor")
                .as_slice(),
        )
        .expect("decode fixture descriptor")
    }

    fn marked_fields(pool: &DescriptorPool, message_name: &str) -> (Vec<MarkedField>, Vec<String>) {
        let message = pool
            .get_message_by_name(message_name)
            .expect("fixture message");
        let extension = pool
            .get_extension_by_name(IDEMPOTENCY_KEY_EXTENSION)
            .expect("idempotency extension");
        let mut marked = Vec::new();
        let mut problems = Vec::new();
        collect_marked_fields(
            &message,
            &extension,
            &mut BTreeSet::new(),
            message.name(),
            None,
            &mut marked,
            &mut problems,
        );
        (marked, problems)
    }

    #[test]
    fn finds_one_key_through_a_singular_oneof_envelope() {
        let pool = fixture_pool(
            r#"
message Request { string client_operation_id = 1 [(heddle.v1.idempotency_key) = true]; }
message Envelope { oneof body { Request request = 1; string heartbeat = 2; } }
"#,
        );
        let (marked, problems) = marked_fields(&pool, "fixture.Envelope");
        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].path, "Envelope.request.client_operation_id");
    }

    #[test]
    fn direct_key_is_authoritative_over_nested_request_keys() {
        let pool = fixture_pool(
            r#"
message FirstRequest { string client_operation_id = 1 [(heddle.v1.idempotency_key) = true]; }
message SecondRequest { string client_operation_id = 1 [(heddle.v1.idempotency_key) = true]; }
message Envelope {
  oneof body {
    FirstRequest first = 1;
    SecondRequest second = 2;
  }
  string client_operation_id = 15 [(heddle.v1.idempotency_key) = true];
}
"#,
        );
        let (marked, problems) = marked_fields(&pool, "fixture.Envelope");
        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].path, "Envelope.client_operation_id");
    }

    #[test]
    fn reports_ambiguous_keys_reachable_by_distinct_paths() {
        let pool = fixture_pool(
            r#"
message Request { string client_operation_id = 1 [(heddle.v1.idempotency_key) = true]; }
message Envelope { Request first = 1; Request second = 2; }
"#,
        );
        let (marked, problems) = marked_fields(&pool, "fixture.Envelope");
        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        assert_eq!(marked.len(), 2);
    }

    #[test]
    fn rejects_wrong_name_type_and_cardinality() {
        let pool = fixture_pool(
            r#"
message BadRequest {
  repeated bytes retry_tokens = 1 [(heddle.v1.idempotency_key) = true];
}
"#,
        );
        let (marked, problems) = marked_fields(&pool, "fixture.BadRequest");
        assert_eq!(marked.len(), 1);
        assert_eq!(problems.len(), 3, "unexpected problems: {problems:?}");
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("named client_operation_id"))
        );
        assert!(problems.iter().any(|problem| problem.contains("singular")));
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("type string"))
        );
    }

    #[test]
    fn rejects_a_second_key_reached_through_a_repeated_child() {
        let pool = fixture_pool(
            r#"
message DirectRequest {
  string client_operation_id = 1 [(heddle.v1.idempotency_key) = true];
}
message NestedRequest {
  string client_operation_id = 1 [(heddle.v1.idempotency_key) = true];
}
message Envelope {
  DirectRequest direct = 1;
  repeated NestedRequest repeated = 2;
}
"#,
        );
        let (marked, problems) = marked_fields(&pool, "fixture.Envelope");
        assert_eq!(marked.len(), 2);
        assert!(problems.iter().any(|problem| {
            problem.contains("Envelope.repeated.client_operation_id")
                && problem.contains("non-singular edge Envelope.repeated")
        }));
    }
}
