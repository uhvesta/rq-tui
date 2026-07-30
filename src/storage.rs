// Compatibility facade for the pre-extraction module path.
//
// Keep this surface deliberately narrow: application code can use the
// persistence service and the records it exchanges, while the implementation
// and migration SQL remain owned by rq-tui-storage.
pub(crate) use rq_tui_storage::{now, PruneOperation, Storage};
