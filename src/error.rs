#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Failed to set up server: {0}")]
    TcpError(std::io::Error),
    #[error("Failed to parse client message: {0}")]
    ClientInvalidJson(serde_json::Error),
    #[error("Nickname already used in lobby")]
    ClientNicknameAlreadyUsed,
    #[error("Lobby already exists")]
    ClientLobbyAlreadyExists,
    #[error("Lobby not found")]
    ClientLobbyNotFound,
    #[error("Lobby full")]
    ClientLobbyFull,
}
