//! Built-in service registry (Ext + Hot).

pub mod chat;
pub mod hot_relay;
pub mod room;

pub use chat::ChatService;
pub use hot_relay::HotRelayService;
pub use room::RoomService;
