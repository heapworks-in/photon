mod agent_review;
mod anchoring;
mod author;
mod checklist;
mod checkout;
mod context;
mod diff;
mod diff_view;
mod functions;
mod gh;
mod github;
mod inbox;
mod open_url;
mod review;
mod review_map;
mod run_locally;
mod since_review;
mod store;
mod suggestions;

pub use agent_review::*;
pub use anchoring::*;
pub use author::*;
pub use checklist::*;
pub use checkout::*;
pub use context::*;
pub use diff::*;
pub use diff_view::*;
pub use functions::*;
pub use gh::*;
pub use github::*;
pub use inbox::*;
pub use open_url::*;
pub use review::*;
pub use review_map::*;
pub use run_locally::*;
pub use since_review::*;
pub use store::*;
pub use suggestions::*;

use gpui::{App, ElementId, SharedString};
use ui::{Button, ButtonCommon, ButtonSize};

pub fn init(cx: &mut App) {
    inbox::init_inbox(cx);
    open_url::init_open_url(cx);
    review::refuse_parent_trust(checkout::review_root(), cx);
}

/// Every text button in the review UI comes from here, so they all get the same height and padding.
pub(crate) fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id, label).size(ButtonSize::Medium)
}
