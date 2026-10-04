mod atom;
pub mod host;
mod state;
pub mod ui;
pub mod ui_ipc;
pub mod ui_server;
mod ui_outputs;
pub mod urid;
mod worker;

/// Bump when host-side behavior changes (bug fixes, feature coverage) so that
/// persisted plugin-health verdicts in the engine are invalidated and plugins
/// are retested. Increment whenever a host fix could unblock a plugin.
pub const HOST_ABI_VERSION: u32 = 3;

pub use atom::{append_atom_event, for_each_event, MidiEvent, TimePosition, EVENT_TRANSFER_URI};
pub use host::{ControlPort, Host, PluginInfo, PluginInstance};
pub use state::{StateEntry, StateProxy};
pub use ui::{find_ui, UiCandidate, UiKind, UiWindow};
pub use ui_ipc::UiMessage;
pub use ui_outputs::SharedOutputs;
pub use ui_outputs::SharedUiInputs;
