// SPDX-License-Identifier: Apache-2.0
//! In-memory views of deleted `heddle.api.v1alpha1` DTOs.
//!
//! The frozen contract moved shared types into `heddle.api.common` and dropped
//! the rest of v1alpha1. Hosted adapters still project v1alpha2 wire records
//! into these shapes for existing client/test code.

#![allow(dead_code, clippy::large_enum_variant)]

use api::heddle::api::common::{RepositoryRef, StateId};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamOpeningProof {
    pub stream_id: String,
    pub route: String,
    pub repository: Option<RepositoryRef>,
    pub resume_cursor: String,
    pub capability_context: Vec<u8>,
    pub nonce: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum RepoEventKind {
    #[default]
    Unspecified = 0,
    DiscussionTurn = 1,
}

impl TryFrom<i32> for RepoEventKind {
    type Error = ();
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unspecified),
            1 => Ok(Self::DiscussionTurn),
            _ => Err(()),
        }
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RepoEvent {
    #[prost(int64, tag = "1")]
    pub event_id: i64,
    #[prost(string, tag = "2")]
    pub repo_id: String,
    #[prost(string, tag = "3")]
    pub event_type: String,
    #[prost(string, tag = "4")]
    pub thread: String,
    #[prost(string, tag = "5")]
    pub ref_name: String,
    #[prost(bool, tag = "6")]
    pub is_thread: bool,
    #[prost(message, optional, tag = "7")]
    pub old_state: Option<StateId>,
    #[prost(message, optional, tag = "8")]
    pub new_state: Option<StateId>,
    #[prost(string, tag = "9")]
    pub actor_subject: String,
    #[prost(string, tag = "14")]
    pub actor_agent_id: String,
    #[prost(string, tag = "10")]
    pub payload_json: String,
    #[prost(message, optional, tag = "11")]
    pub created_at: Option<prost_types::Timestamp>,
    #[prost(string, tag = "13")]
    pub thread_id: String,
    #[prost(int32, tag = "15")]
    pub kind: i32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubscribeRepoEventsRequest {
    pub repo_id: String,
    pub thread: String,
    pub after_event_id: i64,
    pub event_types: Vec<String>,
    pub thread_id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum DiscussionKind {
    #[default]
    Unspecified = 0,
    CodeAnchored = 1,
    Coordination = 2,
    Imported = 3,
}

impl TryFrom<i32> for DiscussionKind {
    type Error = ();
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unspecified),
            1 => Ok(Self::CodeAnchored),
            2 => Ok(Self::Coordination),
            3 => Ok(Self::Imported),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathSymbolRef {
    pub file: String,
    pub symbol: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscussionTurn {
    pub author_name: String,
    pub author_email: String,
    pub body: String,
    pub turn_id: String,
    pub turn_seq: u64,
    pub posted_at: Option<prost_types::Timestamp>,
}

pub mod discussion_resolution {
    use api::heddle::api::common::StateId;

    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Open {}
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct ResolvedIntoAnnotation {
        pub annotation_id: String,
    }
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct ResolvedByEdit {
        pub state_id: Option<StateId>,
    }
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Dismissed {
        pub reason: String,
    }
    #[derive(Clone, Debug, PartialEq)]
    pub enum State {
        Open(Open),
        IntoAnnotation(ResolvedIntoAnnotation),
        ByEdit(ResolvedByEdit),
        Dismissed(Dismissed),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscussionResolution {
    pub state: Option<discussion_resolution::State>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Discussion {
    pub id: String,
    pub anchor: Option<PathSymbolRef>,
    pub opened_against_state: Option<StateId>,
    pub thread_ref: String,
    pub turns: Vec<DiscussionTurn>,
    pub resolution: Option<DiscussionResolution>,
    pub visibility: String,
    pub thread_id: String,
    pub kind: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum ContextAnnotationKind {
    #[default]
    Unspecified = 0,
    Constraint = 1,
    Invariant = 2,
    Rationale = 3,
}

impl TryFrom<i32> for ContextAnnotationKind {
    type Error = ();
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unspecified),
            1 => Ok(Self::Constraint),
            2 => Ok(Self::Invariant),
            3 => Ok(Self::Rationale),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum ContextAnnotationStatus {
    #[default]
    Unspecified = 0,
    Active = 1,
    Superseded = 2,
}

impl TryFrom<i32> for ContextAnnotationStatus {
    type Error = ();
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unspecified),
            1 => Ok(Self::Active),
            2 => Ok(Self::Superseded),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SymbolScope {
    pub name: String,
    pub resolved_start: Option<u32>,
    pub resolved_end: Option<u32>,
}

pub mod annotation_scope {
    #[derive(Clone, Debug, PartialEq)]
    pub enum Scope {
        File(bool),
        Symbol(super::SymbolScope),
        Lines(super::LineRange),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnnotationScope {
    pub scope: Option<annotation_scope::Scope>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextAnnotation {
    pub id: String,
    pub scope: Option<AnnotationScope>,
    pub content: String,
    pub tags: Vec<String>,
    pub attribution: String,
    pub created_at: Option<prost_types::Timestamp>,
    pub source_hash: Option<Vec<u8>>,
    pub created_at_state: Option<StateId>,
    pub status: i32,
    pub kind: i32,
    pub revision_count: u32,
    pub supersedes_annotation_id: Option<String>,
    pub supersedes_rewrite_pct: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextRevision {
    pub revision_id: String,
    pub kind: i32,
    pub content: String,
    pub tags: Vec<String>,
    pub attribution: String,
    pub created_at: Option<prost_types::Timestamp>,
    pub source_hash: Option<Vec<u8>>,
    pub created_at_state: Option<StateId>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnnotatedFile {
    pub path: String,
    pub annotations: Vec<ContextAnnotation>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StateContextEntry {
    pub state_id: Option<StateId>,
    pub annotations: Vec<ContextAnnotation>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListContextSuggestionsResponse {}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SetContextResponse {
    pub state: Option<StateId>,
    pub annotations: u32,
    pub annotation_id: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReviseContextResponse {
    pub state: Option<StateId>,
    pub revisions: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SupersedeContextResponse {
    pub state: Option<StateId>,
    pub new_annotation_id: String,
    pub rewrite_pct: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum ReviewKind {
    #[default]
    Unspecified = 0,
    Read = 1,
    AgentPreview = 2,
    AgentCoReview = 3,
}

pub mod review_scope {
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct WholeChange {}
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct SymbolList {
        pub symbols: Vec<super::PathSymbolRef>,
    }
    #[derive(Clone, Debug, PartialEq)]
    pub enum Scope {
        WholeChange(WholeChange),
        Symbols(SymbolList),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReviewScope {
    pub scope: Option<review_scope::Scope>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SignStateResponse {
    pub signature_id: String,
    pub state_id: Option<StateId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum TransportMode {
    #[default]
    Unspecified = 0,
    NativePack = 1,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TransferCheckpoint {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
    #[prost(int32, tag = "2")]
    pub transport_mode: i32,
    #[prost(uint64, tag = "3")]
    pub resume_offset: u64,
    #[prost(uint32, tag = "4")]
    pub chunk_index: u32,
    #[prost(bytes = "vec", tag = "5")]
    pub checkpoint: Vec<u8>,
    #[prost(bool, tag = "6")]
    pub is_complete: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RefEntry {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub state_id: Option<StateId>,
    #[prost(bool, tag = "3")]
    pub is_thread: bool,
    #[prost(string, tag = "4")]
    pub revision_address: String,
    #[prost(string, tag = "5")]
    pub thread_id: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PullReady {
    #[prost(message, optional, tag = "1")]
    pub remote_state: Option<StateId>,
    #[prost(message, optional, tag = "3")]
    pub transfer: Option<TransferCheckpoint>,
    #[prost(bool, tag = "6")]
    pub full_closure_available: bool,
    #[prost(uint32, tag = "9")]
    pub owner_authorization_protocol_version: u32,
    #[prost(message, optional, tag = "10")]
    pub owner_genesis: Option<api::heddle::api::v1alpha2::SignedSpoolOwnerGenesis>,
    #[prost(message, repeated, tag = "11")]
    pub refs: Vec<RefEntry>,
    #[prost(string, tag = "12")]
    pub head_thread: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PullComplete {
    #[prost(bool, tag = "1")]
    pub success: bool,
    #[prost(message, optional, tag = "2")]
    pub new_state: Option<StateId>,
    #[prost(string, tag = "3")]
    pub error: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum PackStreamKind {
    #[default]
    Unspecified = 0,
    Pack = 1,
    Index = 2,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PackChunk {
    #[prost(int32, tag = "1")]
    pub stream_kind: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub data: Vec<u8>,
    #[prost(message, optional, tag = "3")]
    pub transfer: Option<TransferCheckpoint>,
    #[prost(uint32, tag = "4")]
    pub chunk_length: u32,
    #[prost(bool, tag = "5")]
    pub is_final_chunk: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PullServerFrame {
    #[prost(oneof = "pull_server_frame::Frame", tags = "1, 2, 3")]
    pub frame: Option<pull_server_frame::Frame>,
}

pub mod pull_server_frame {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Frame {
        #[prost(message, tag = "1")]
        Ready(super::PullReady),
        #[prost(message, tag = "2")]
        Pack(super::PackChunk),
        #[prost(message, tag = "3")]
        Complete(super::PullComplete),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ObjectDescriptor {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PushRequest {
    #[prost(message, repeated, tag = "6")]
    pub objects: Vec<ObjectDescriptor>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PushReady {
    #[prost(message, repeated, tag = "3")]
    pub want_objects: Vec<ObjectDescriptor>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PushComplete {
    #[prost(bool, tag = "1")]
    pub success: bool,
    #[prost(string, tag = "3")]
    pub error: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PushServerFrame {
    #[prost(oneof = "push_server_frame::Frame", tags = "1, 2")]
    pub frame: Option<push_server_frame::Frame>,
}

pub mod push_server_frame {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Frame {
        #[prost(message, tag = "1")]
        Ready(super::PushReady),
        #[prost(message, tag = "2")]
        Complete(super::PushComplete),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PushClientFrame {
    #[prost(oneof = "push_client_frame::Frame", tags = "1, 2, 3")]
    pub frame: Option<push_client_frame::Frame>,
}

pub mod push_client_frame {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Frame {
        #[prost(message, tag = "2")]
        Request(Box<super::PushRequest>),
        #[prost(message, tag = "3")]
        Pack(super::PackChunk),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListRefsPageEnd {
    #[prost(string, tag = "1")]
    pub next_page_token: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListRefsResponse {
    #[prost(oneof = "list_refs_response::Frame", tags = "3, 5")]
    pub frame: Option<list_refs_response::Frame>,
}

pub mod list_refs_response {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Frame {
        #[prost(message, tag = "3")]
        Item(super::RefEntry),
        #[prost(message, tag = "5")]
        PageEnd(super::ListRefsPageEnd),
    }
}
