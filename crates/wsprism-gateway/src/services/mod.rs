//! Built-in service registry (Ext + Hot).

pub mod chat;
pub mod echo_binary;
pub mod room;

pub use chat::ChatService;
pub use echo_binary::EchoBinaryService;
pub use room::RoomService;
