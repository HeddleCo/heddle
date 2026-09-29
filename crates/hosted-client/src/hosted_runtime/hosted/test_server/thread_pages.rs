//! Weft's shared Thread page admission rule, used by the native hosted fixtures.
use api::heddle::api::{
    common::{CallFailure, CallFailureCode},
    v1alpha2::{ObservationMode, ObserveThreadRequest, ThreadSection},
};

/// Returns capture, evidence, review and collaboration sizes after defaults.
/// Mirrors weft `crates/weft-hosted/src/server/hosted/observation.rs::Query::new`.
pub fn thread_section_page_sizes(
    request: &ObserveThreadRequest,
    default_max_items: u32,
) -> Result<[u32; 4], CallFailure> {
    let options = request.observe.clone().unwrap_or_default();
    let max_items = options
        .budget
        .filter(|budget| budget.max_items != 0)
        .map_or(default_max_items, |budget| budget.max_items);
    let follow = options.mode != ObservationMode::Once as i32;
    let selected = |section| request.sections.contains(&(section as i32));
    let active = [
        selected(ThreadSection::Captures) || request.sections.is_empty(),
        selected(ThreadSection::Evidence),
        selected(ThreadSection::Review),
        selected(ThreadSection::Collaboration),
    ];
    let sharing = selected(ThreadSection::Sharing);
    let count = active.iter().filter(|&&value| value).count() as u32;
    let overhead = 1
        + u32::from(active[2])
        + u32::from(sharing)
        + if follow {
            count + u32::from(sharing)
        } else {
            0
        };
    let capacity = max_items.saturating_sub(overhead);
    let default_size = (capacity / count.max(1)).min(64);
    let pages = request.pages.clone().unwrap_or_default();
    let pages = [
        pages.captures,
        pages.evidence,
        pages.reviews,
        pages.collaboration,
    ];
    let mut sizes = [0; 4];
    for (index, page) in pages.into_iter().enumerate() {
        let page = page.unwrap_or_default();
        if !active[index] && (page.size != 0 || !page.after_page.is_empty()) {
            return Err(CallFailure {
                code: CallFailureCode::InvalidArgument as i32,
                message: "pagination requires its Thread section".into(),
                error: None,
            });
        }
        if active[index] {
            sizes[index] = if page.size == 0 {
                default_size
            } else {
                page.size
            };
        }
    }
    if sizes
        .iter()
        .fold(0u32, |sum, size| sum.saturating_add(*size))
        > capacity
        || active
            .iter()
            .zip(sizes)
            .any(|(&active, size)| active && size == 0)
        || sizes[1] > 256
    {
        return Err(CallFailure {
            code: CallFailureCode::ResourceExhausted as i32,
            message: "Thread section pages exceed shared item budget".into(),
            error: None,
        });
    }
    Ok(sizes)
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha2::{ObserveOptions, PageRequest, ThreadPages};

    use super::*;

    #[test]
    fn weft_shared_budget_counts_all_sections_and_overhead() {
        let mut request = ObserveThreadRequest {
            sections: vec![
                ThreadSection::Captures as i32,
                ThreadSection::Evidence as i32,
                ThreadSection::Review as i32,
                ThreadSection::Collaboration as i32,
            ],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Once as i32,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            thread_section_page_sizes(&request, 64).expect("default pages"),
            [15; 4]
        );
        request.pages = Some(ThreadPages {
            captures: Some(PageRequest {
                size: 16,
                ..Default::default()
            }),
            evidence: Some(PageRequest {
                size: 16,
                ..Default::default()
            }),
            reviews: Some(PageRequest {
                size: 16,
                ..Default::default()
            }),
            collaboration: Some(PageRequest {
                size: 16,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(thread_section_page_sizes(&request, 64).is_err());
        request.pages = None;
        request.observe = None;
        assert_eq!(
            thread_section_page_sizes(&request, 64).expect("follow pages"),
            [14; 4]
        );
        request.sections = vec![ThreadSection::Review as i32];
        request.observe = Some(ObserveOptions {
            mode: ObservationMode::Once as i32,
            ..Default::default()
        });
        assert_eq!(
            thread_section_page_sizes(&request, 64).expect("review page"),
            [0, 0, 62, 0]
        );
        assert_eq!(
            thread_section_page_sizes(&request, 10).expect("smaller endpoint"),
            [0, 0, 8, 0]
        );
        assert!(thread_section_page_sizes(&request, 2).is_err());
    }
}
