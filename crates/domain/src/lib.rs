mod diff;
mod domain;

pub use diff::{parse_unified, DiffFile, DiffLine, DiffSet, FileStatus, Hunk, LineKind};
pub use domain::{
    AnchorSide, Annotation, AnnotationKind, AskMessage, BaseBranchSource,
    DeliveryState, EphemeralSessionRecord, PendingChat, Placement, Repo, ReviewContext,
    SessionRecord, Version, VersionKind, WorkItem,
};

#[cfg(test)]
mod tests {
    use super::{BaseBranchSource, DeliveryState, ReviewContext, VersionKind};

    #[test]
    fn enum_values_round_trip_through_the_domain_boundary() {
        assert_eq!(BaseBranchSource::try_from("per_repo").unwrap().as_str(), "per_repo");
        assert_eq!(VersionKind::try_from("working_tree").unwrap().as_str(), "working_tree");
        assert_eq!(DeliveryState::try_from("pending").unwrap().as_str(), "pending");
    }

    #[test]
    fn invalid_enum_values_are_rejected() {
        assert!(BaseBranchSource::try_from("not-a-source").is_err());
        assert!(VersionKind::try_from("not-a-version").is_err());
        assert!(DeliveryState::try_from("not-a-state").is_err());
    }

    #[test]
    fn review_context_defaults_to_an_unattached_draft() {
        let context = ReviewContext::default();
        assert!(!context.attached_to_session);
        assert_eq!(context.delivery_state, DeliveryState::Draft);
        assert!(context.title.is_empty());
    }
}
