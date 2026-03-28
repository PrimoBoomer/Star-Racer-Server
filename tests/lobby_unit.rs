use star_racer_server::lobby::Lobby;

// ── Construction ──────────────────────────────────────────────────────────────

#[test]
fn new_lobby_has_zero_players() {
    let lobby = Lobby::new("TestLobby".into(), "Alice".into(), "12:00".into(), 2, 6);
    assert_eq!(lobby.player_count(), 0);
}

#[test]
fn new_lobby_is_not_racing() {
    let lobby = Lobby::new("TestLobby".into(), "Alice".into(), "12:00".into(), 2, 6);
    assert!(!lobby.is_racing());
}

#[test]
fn new_lobby_with_min_one_is_not_racing() {
    let lobby = Lobby::new("Solo".into(), "Alice".into(), "00:00".into(), 1, 4);
    assert!(!lobby.is_racing());
    assert_eq!(lobby.player_count(), 0);
}

// ── update() with no racers ───────────────────────────────────────────────────

#[tokio::test]
async fn update_with_no_racers_returns_false() {
    let mut lobby = Lobby::new("TestLobby".into(), "Alice".into(), "12:00".into(), 2, 6);
    assert!(!lobby.update(0.016).await);
}

#[tokio::test]
async fn update_with_zero_delta_returns_false_when_empty() {
    let mut lobby = Lobby::new("TestLobby".into(), "Alice".into(), "12:00".into(), 1, 4);
    assert!(!lobby.update(0.0).await);
}

#[tokio::test]
async fn repeated_updates_with_no_racers_keep_returning_false() {
    let mut lobby = Lobby::new("TestLobby".into(), "Alice".into(), "12:00".into(), 2, 4);
    for _ in 0..20 {
        assert!(!lobby.update(0.016).await);
    }
}
