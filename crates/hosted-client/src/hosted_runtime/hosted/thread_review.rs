//! Exact native review snapshot and signed mutation transport.
use api::heddle::api::v1alpha2 as contract;
use thread_api::rpc;
use wire::ProtocolError;

use super::HostedClient;

pub struct ReviewSnapshot {
    pub endpoint_key: Vec<u8>,
    pub overview: contract::ThreadOverview,
    pub comparison: Option<contract::ReviewComparison>,
    pub decisions: Vec<contract::ReviewDecision>,
}

impl HostedClient {
    pub async fn observe_review(
        &self,
        spool_address: &str,
        thread_name: &str,
    ) -> Result<ReviewSnapshot, ProtocolError> {
        self.observe_review_with_target(spool_address, thread_name, None)
            .await
    }

    pub async fn observe_landing_assessment(
        &self,
        spool_address: &str,
        thread_name: &str,
        target_name: &str,
    ) -> Result<ReviewSnapshot, ProtocolError> {
        self.observe_review_with_target(spool_address, thread_name, Some(target_name))
            .await
    }

    async fn observe_review_with_target(
        &self,
        spool_address: &str,
        thread_name: &str,
        target_name: Option<&str>,
    ) -> Result<ReviewSnapshot, ProtocolError> {
        let reference = self.resolve_thread_ref(spool_address, thread_name).await?;
        let target = match target_name {
            Some(name) => Some(self.resolve_thread_ref(spool_address, name).await?),
            None => None,
        };
        let remote = self.native().await.map_err(native_error)?;
        let mut after_page = Vec::new();
        let mut overview: Option<contract::ThreadOverview> = None;
        let mut comparison: Option<contract::ReviewComparison> = None;
        let mut decisions = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let mut observation = remote
                .observe::<rpc::ThreadServiceObserveThread>(
                    contract::ObserveThreadRequest {
                        thread: Some(reference.clone()),
                        landing_target: target.clone(),
                        sections: vec![
                            contract::ThreadSection::Overview as i32,
                            contract::ThreadSection::Review as i32,
                        ],
                        pages: Some(contract::ThreadPages {
                            reviews: Some(contract::PageRequest {
                                size: 128,
                                after_page: after_page.clone(),
                            }),
                            ..Default::default()
                        }),
                        observe: Some(contract::ObserveOptions {
                            mode: contract::ObservationMode::Once as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .map_err(native_error)?;
            let batch = observation
                .next_commit()
                .await
                .map_err(native_error)?
                .ok_or_else(|| {
                    ProtocolError::InvalidState(
                        "review observation ended without checkpoint".into(),
                    )
                })?;
            let mut page_overview = None;
            let mut page_comparison = None;
            let mut page_status = None;
            for change in batch.changes {
                match change {
                    contract::thread_event::Payload::Overview(value) => {
                        if page_overview.replace(value).is_some() {
                            return Err(invalid("review page has duplicate Thread overview"));
                        }
                    }
                    contract::thread_event::Payload::Comparison(value) => {
                        if page_comparison.replace(value).is_some() {
                            return Err(invalid("review page has duplicate comparison"));
                        }
                    }
                    contract::thread_event::Payload::Review(value) => {
                        let decision = value
                            .decision
                            .ok_or_else(|| invalid("review decision absent"))?;
                        if value.original.as_ref().is_none_or(|original| {
                            original.canonical_record.is_empty() || original.signatures.is_empty()
                        }) {
                            return Err(invalid("review original proof absent"));
                        }
                        let id = decision
                            .r#ref
                            .as_ref()
                            .ok_or_else(|| invalid("review has no ID"))?;
                        if id.spool != reference.spool
                            || uuid::Uuid::parse_str(&id.id).is_err()
                            || decision.thread.as_ref() != Some(&reference)
                            || !seen.insert(id.id.clone())
                            || decisions.len() >= 4096
                        {
                            return Err(invalid(
                                "review page has invalid, duplicate or excess row",
                            ));
                        }
                        decisions.push(decision);
                    }
                    contract::thread_event::Payload::Status(value) if value.section == "review" => {
                        if page_status.replace(value).is_some() {
                            return Err(invalid("review page has duplicate status"));
                        }
                    }
                    _ => {}
                }
            }
            let page_overview = page_overview.ok_or_else(|| invalid("Thread overview absent"))?;
            if page_overview.r#ref.as_ref() != Some(&reference) || page_overview.version.is_empty()
            {
                return Err(invalid("review overview names another Thread"));
            }
            if let Some(target) = &target {
                let assessment = page_overview
                    .landing_assessment
                    .as_ref()
                    .ok_or_else(|| invalid("selected landing target has no assessment"))?;
                if assessment.target.as_ref() != Some(target)
                    || assessment
                        .source
                        .as_ref()
                        .is_none_or(|source| source.spool != reference.spool)
                    || assessment
                        .expected_target
                        .as_ref()
                        .is_none_or(|head| head.spool != target.spool)
                    || assessment.policy_version.is_empty()
                {
                    return Err(invalid(
                        "landing assessment is incomplete or names another target",
                    ));
                }
            } else if page_overview.landing_assessment.is_some() {
                return Err(invalid("unrequested landing assessment supplied"));
            }
            if let Some(previous) = &overview {
                if previous.version != page_overview.version
                    || previous.landing_assessment != page_overview.landing_assessment
                {
                    return Err(invalid("Thread changed during review paging; retry"));
                }
            } else {
                overview = Some(page_overview);
            }
            if let Some(value) = page_comparison {
                if value.source.is_none() || value.base.is_none() || value.policy_version.is_empty()
                {
                    return Err(invalid("review comparison is incomplete"));
                }
                if comparison.as_ref().is_some_and(|prior| prior != &value) {
                    return Err(invalid("review comparison changed during paging; retry"));
                }
                comparison = Some(value);
            }
            let status = page_status.ok_or_else(|| invalid("review status absent"))?;
            let page = status
                .page
                .ok_or_else(|| invalid("review page metadata absent"))?;
            if page.exhausted {
                if status.coverage != contract::Coverage::Complete as i32
                    && !(comparison.is_none()
                        && status.coverage == contract::Coverage::Partial as i32)
                {
                    return Err(invalid("review coverage is incomplete"));
                }
                return Ok(ReviewSnapshot {
                    endpoint_key: remote
                        .description
                        .endpoint
                        .as_ref()
                        .ok_or_else(|| invalid("hosted endpoint identity absent"))?
                        .public_key
                        .clone(),
                    overview: overview.ok_or_else(|| invalid("Thread overview absent"))?,
                    comparison,
                    decisions,
                });
            }
            if status.coverage != contract::Coverage::Partial as i32
                || page.next_page.is_empty()
                || page.next_page == after_page
            {
                return Err(invalid("review cursor did not advance"));
            }
            after_page = page.next_page;
        }
    }

    pub async fn record_review(
        &self,
        request: &contract::RecordReviewRequest,
    ) -> Result<(), ProtocolError> {
        let remote = self.native().await.map_err(native_error)?;
        let response = remote
            .api
            .call::<rpc::ThreadServiceRecordReview>(request)
            .await
            .map_err(super::helpers::native_client_error)?;
        super::user::require_applied_receipt(
            response.receipt,
            &request.client_operation_id,
            &remote.description.endpoint,
            "review decision",
        )
    }
}

fn invalid(message: &str) -> ProtocolError {
    ProtocolError::InvalidState(message.into())
}

fn native_error(error: impl std::fmt::Display) -> ProtocolError {
    invalid(&error.to_string())
}
