//! Reusable, style-neutral building blocks. Visuals come from CSS tokens in `style/app.css`.

pub mod avatar;
pub mod command_bar;
pub mod empty_state;
pub mod icon;
pub mod person_card;
pub mod presence_picker;
pub mod spinner;
pub mod split_pane;
pub mod toasts;
pub mod virtual_list;

pub use avatar::{Avatar, Presence};
pub use command_bar::{CommandBar, CommandButton};
pub use empty_state::EmptyState;
pub use icon::Icon;
pub use spinner::Spinner;
pub use split_pane::SplitPane;
pub use toasts::Toasts;
pub use virtual_list::VirtualList;
