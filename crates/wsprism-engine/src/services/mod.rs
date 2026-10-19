//! Built-in service registry (Ext + Hot).

mod chat;
mod hot_relay;
mod room;

pub use chat::ChatService;
pub use hot_relay::HotRelayService;
pub use room::RoomService;
