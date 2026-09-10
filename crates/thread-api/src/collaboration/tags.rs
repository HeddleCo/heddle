//! Lossless annotation metadata and query conversion at the shared API boundary.
use heddle_object_model::object as model;

use crate::{contract as api, transport::Error};

fn invalid(_: impl std::fmt::Display) -> Error {
    Error::Protocol("invalid structured annotation metadata or query")
}
fn required<T>(value: Option<T>) -> Result<T, Error> {
    value.ok_or(Error::Protocol("annotation field required"))
}

pub fn annotation_value(value: &api::AnnotationValue) -> Result<model::AnnotationValue, Error> {
    use api::annotation_value::Value;
    let value = match required(value.value.as_ref())? {
        Value::Text(v) => model::AnnotationValue::Text(v.clone()),
        Value::Boolean(v) => model::AnnotationValue::Boolean(*v),
        Value::Integer(v) => model::AnnotationValue::Integer(*v),
        Value::Decimal(v) => model::AnnotationValue::Decimal(model::AnnotationDecimal {
            coefficient: v.coefficient,
            scale: v.scale,
        }),
    };
    value.validate().map_err(invalid)?;
    Ok(value)
}
pub fn annotation_value_ref(value: &model::AnnotationValue) -> api::AnnotationValue {
    use api::annotation_value::Value;
    api::AnnotationValue {
        value: Some(match value {
            model::AnnotationValue::Text(v) => Value::Text(v.clone()),
            model::AnnotationValue::Boolean(v) => Value::Boolean(*v),
            model::AnnotationValue::Integer(v) => Value::Integer(*v),
            model::AnnotationValue::Decimal(v) => Value::Decimal(api::AnnotationDecimal {
                coefficient: v.coefficient,
                scale: v.scale,
            }),
        }),
    }
}
pub fn annotation_source(
    value: &api::AnnotationSourceReference,
) -> Result<model::AnnotationSourceReference, Error> {
    let source = required(value.source.as_ref())?;
    let revision = required(source.revision.as_ref())?;
    let spool = required(revision.spool.as_ref())?
        .id
        .parse::<uuid::Uuid>()
        .map_err(invalid)?;
    let thread = source
        .thread
        .as_ref()
        .map(|t| {
            if required(t.spool.as_ref())?
                .id
                .parse::<uuid::Uuid>()
                .map_err(invalid)?
                != spool
            {
                return Err(Error::Protocol(
                    "annotation source Thread belongs to another spool",
                ));
            }
            Ok(model::ContentHash::from_bytes(
                required(t.id.as_ref())?
                    .value
                    .as_slice()
                    .try_into()
                    .map_err(invalid)?,
            ))
        })
        .transpose()?;
    let reference = model::AnnotationSourceReference {
        scope: model::CollaborationScope { spool, thread },
        source: model::CollaborationSourceAnchor {
            revision: match required(revision.revision.as_ref())? {
                api::revision_ref::Revision::State(id) => model::CollaborationRevision::State {
                    state_id: model::StateId::from_bytes(
                        id.value.as_slice().try_into().map_err(invalid)?,
                    ),
                },
                api::revision_ref::Revision::GitCommitOid(oid) => {
                    model::CollaborationRevision::GitCommit { oid: oid.clone() }
                }
            },
            path: source.path.clone(),
            symbol_id: source.symbol_id.clone(),
            start_line: source.start_line,
            end_line: source.end_line,
            target: source
                .target
                .as_ref()
                .map(super::references::source_target)
                .transpose()?,
        },
    };
    reference.validate().map_err(invalid)?;
    Ok(reference)
}
pub fn annotation_source_ref(
    value: &model::AnnotationSourceReference,
) -> api::AnnotationSourceReference {
    let spool = Some(api::SpoolRef {
        id: value.scope.spool.to_string(),
    });
    api::AnnotationSourceReference {
        source: Some(api::SourceAnchor {
            revision: Some(api::RevisionRef {
                spool: spool.clone(),
                revision: Some(match &value.source.revision {
                    model::CollaborationRevision::State { state_id } => {
                        api::revision_ref::Revision::State(::api::heddle::api::v1alpha1::StateId {
                            value: state_id.as_bytes().to_vec(),
                        })
                    }
                    model::CollaborationRevision::GitCommit { oid } => {
                        api::revision_ref::Revision::GitCommitOid(oid.clone())
                    }
                }),
            }),
            path: value.source.path.clone(),
            symbol_id: value.source.symbol_id.clone(),
            start_line: value.source.start_line,
            end_line: value.source.end_line,
            target: value
                .source
                .target
                .as_ref()
                .map(super::references::source_target_ref),
            thread: value.scope.thread.map(|id| api::ThreadRef {
                spool,
                id: Some(api::ThreadId {
                    value: id.as_bytes().to_vec(),
                }),
            }),
        }),
    }
}
pub fn annotation_tag(value: &api::AnnotationTag) -> Result<model::AnnotationTag, Error> {
    use api::annotation_tag::Tag;
    let tag = match required(value.tag.as_ref())? {
        Tag::Text(text) => model::AnnotationTag::Text { text: text.clone() },
        Tag::Symbol(v) => model::AnnotationTag::Symbol {
            name: v.name.clone(),
            target: v.target.as_ref().map(annotation_source).transpose()?,
        },
        Tag::Source(v) => model::AnnotationTag::Source {
            target: annotation_source(v)?,
        },
        Tag::Entity(v) => model::AnnotationTag::Entity {
            target: super::mention(v)?,
        },
        Tag::Property(v) => model::AnnotationTag::Property {
            key: v.key.clone(),
            value: annotation_value(required(v.value.as_ref())?)?,
        },
    };
    tag.validate().map_err(invalid)?;
    Ok(tag)
}
pub fn annotation_tag_ref(value: &model::AnnotationTag) -> api::AnnotationTag {
    use api::annotation_tag::Tag;
    api::AnnotationTag {
        tag: Some(match value {
            model::AnnotationTag::Text { text } => Tag::Text(text.clone()),
            model::AnnotationTag::Symbol { name, target } => {
                Tag::Symbol(api::AnnotationSymbolTag {
                    name: name.clone(),
                    target: target.as_ref().map(annotation_source_ref),
                })
            }
            model::AnnotationTag::Source { target } => Tag::Source(annotation_source_ref(target)),
            model::AnnotationTag::Entity { target } => Tag::Entity(super::mention_ref(target)),
            model::AnnotationTag::Property { key, value } => {
                Tag::Property(api::AnnotationProperty {
                    key: key.clone(),
                    value: Some(annotation_value_ref(value)),
                })
            }
        }),
    }
}
pub fn annotation_tags(values: &[api::AnnotationTag]) -> Result<Vec<model::AnnotationTag>, Error> {
    if values.len() > 128 {
        return Err(Error::Protocol("at most 128 annotation tags"));
    }
    let values = values
        .iter()
        .map(annotation_tag)
        .collect::<Result<Vec<_>, _>>()?;
    model::validate_annotation_tags(&values).map_err(invalid)?;
    Ok(values)
}
pub fn annotation_query(value: &api::AnnotationQuery) -> Result<model::AnnotationQuery, Error> {
    if value
        .all
        .len()
        .saturating_add(value.any.len())
        .saturating_add(value.none.len())
        > 64
    {
        return Err(Error::Protocol("at most 64 annotation predicates"));
    }
    let query = model::AnnotationQuery {
        all: value.all.iter().map(predicate).collect::<Result<_, _>>()?,
        any: value.any.iter().map(predicate).collect::<Result<_, _>>()?,
        none: value.none.iter().map(predicate).collect::<Result<_, _>>()?,
    };
    query.validate().map_err(invalid)?;
    Ok(query)
}
fn predicate(value: &api::AnnotationTagPredicate) -> Result<model::AnnotationTagPredicate, Error> {
    use api::annotation_tag_predicate::Predicate as P;
    use model::AnnotationTagPredicate as M;
    Ok(match required(value.predicate.as_ref())? {
        P::Exact(tag) => M::Exact {
            tag: annotation_tag(tag)?,
        },
        P::SymbolName(name) => M::SymbolName { name: name.clone() },
        P::FilePath(path) => M::FilePath { path: path.clone() },
        P::FilePrefix(path) => M::FilePrefix { path: path.clone() },
        P::LinesOverlap(target) => M::LinesOverlap {
            target: annotation_source(target)?,
        },
        P::Property(v) => {
            use api::annotation_property_predicate::Comparison as C;
            use model::AnnotationComparison as M;
            let comparison = match C::try_from(v.comparison).map_err(invalid)? {
                C::Equal => M::Equal,
                C::Less => M::Less,
                C::LessOrEqual => M::LessOrEqual,
                C::Greater => M::Greater,
                C::GreaterOrEqual => M::GreaterOrEqual,
                C::Exists => M::Exists,
                C::Unspecified => return Err(Error::Protocol("property comparison required")),
            };
            model::AnnotationTagPredicate::Property {
                key: v.key.clone(),
                comparison,
                value: v.value.as_ref().map(annotation_value).transpose()?,
            }
        }
    })
}
