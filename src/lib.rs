mod api;
mod brief;
mod cache;
#[cfg(feature = "cli")]
mod cli;
mod explore;
mod explore_api;
mod frames;
#[cfg(feature = "cli")]
mod install;
mod model;
mod mutate;
pub mod ops;
mod search;
mod source;
mod store;
pub mod wiki;

pub use api::{
    BriefResponse, CheckpointReport, Cm, ExportReport, FeedbackVerdict, FinishResult, ImportReport,
    NewMemory, ReindexReport, RememberResult, StoreStatus, VerifyReport,
};
pub use explore_api::{
    ExploreCursor, ExploreMode, ExploreOptions, ExplorePlan, ExploreTarget, MechanicalPlan,
};
pub use frames::TaskFrame;
pub use mutate::{AbsorbOptions, AbsorbReport};
pub use ops::{
    append_input, append_json, audit, cache_stats, checkpoint as checkpoint_op, claim, conflicts,
    doctor, export_text, feedback, finish_with, forget, frame_get_value, frame_list_value,
    frame_set, get_memory, govern, import_text, init as init_op, merge, pack_prompt, purge,
    reindex as reindex_op, release, remember_with, scan, similar, verify as verify_op,
    AuditOptions, BriefOptions, CandidateOut, ConflictGroupOut, ConsolidateOut, ExportFormat,
    FinishOptions, FrameSetOptions, GovernOptions, MergeGroupOut, RememberOptions, RememberOutcome,
    SimilarOut, SKILL_TEXT,
};
pub use search::{RecallHit, RecallQuery, RecallResponse, SearchHit, SearchOut, SearchQuery};
pub use store::{SourcePolicy, StoreOptions};
pub use wiki::{
    WikiCheckReport, WikiContextPlan, WikiExcerpt, WikiExcerptSection, WikiGenerateReport,
    WikiManifest, WikiPage, WikiPageMatch, WikiPageState, WikiPlan, WikiRememberReport, WikiReview,
    WikiStatus,
};

#[cfg(feature = "cli")]
pub use cli::run;
pub use model::{AppendInput, Memory, MemoryOut};

/// The cm-memory crate version (the `cm` binary prints the same value).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
