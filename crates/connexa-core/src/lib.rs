//! Constants shared by every Connexa component.

pub const PROJECT_NAME: &str = "Connexa";

/// Version of the signaling protocol spoken over the WebSocket.
/// Bump when a change is not backwards compatible.
pub const PROTOCOL_VERSION: u32 = 1;

/// Number of digits in a room code.
pub const ROOM_CODE_LEN: usize = 9;

/// MVP mesh size. Configurable at runtime; larger rooms will need an SFU.
pub const DEFAULT_MAX_PARTICIPANTS: usize = 3;

/// Maximum length of a user supplied display name, in characters.
pub const MAX_DISPLAY_NAME_CHARS: usize = 32;
