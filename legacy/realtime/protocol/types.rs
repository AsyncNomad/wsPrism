use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoomId(pub String);

impl SessionId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
}
