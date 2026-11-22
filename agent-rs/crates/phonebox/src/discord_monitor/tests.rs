//! `DiscordTests`: control and notifications follow the widget's options, images only for what the agent reported, a
//! PING is answered; the pipe is an in-memory duplex.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

struct Setup {
    _dir: tempfile::TempDir,
    store: Arc<ConfigStore>,
    path: PathBuf,
}

fn config(
    control: Option<bool>,
    page_enabled: bool,
    notifications: Option<bool>,
    discord_enabled: bool,
) -> Value {
    let mut options = serde_json::Map::new();
    if let Some(c) = control {
        options.insert("control".into(), json!(c));
    }
    if let Some(n) = notifications {
        options.insert("notifications".into(), json!(n));
    }
    json!({
        "discord": {"enabled": discord_enabled, "clientId": "1", "clientSecret": "s"},
        // at least one page stays enabled (the config repairs an all-disabled list)
        "pages": [
            {"id": "p", "title": "P", "enabled": page_enabled,
             "widgets": [{"id": "d", "type": "discord", "x": 0, "y": 0, "w": 4, "h": 5, "options": options}]},
            {"id": "q", "title": "Q", "enabled": true, "widgets": []},
        ],
    })
}

fn setup(
    control: Option<bool>,
    page_enabled: bool,
    notifications: Option<bool>,
    discord_enabled: bool,
) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        config(control, page_enabled, notifications, discord_enabled).to_string(),
    )
    .unwrap();
    Setup {
        store: Arc::new(ConfigStore::new(path.clone())),
        _dir: dir,
        path,
    }
}

fn monitor(control: Option<bool>) -> (Setup, DiscordMonitor) {
    let s = setup(control, true, None, true);
    let m = DiscordMonitor::new(s.store.clone());
    (s, m)
}

fn polled() -> DiscordSnapshot {
    DiscordSnapshot {
        connected: true,
        user: Some("me".into()),
        ..DiscordSnapshot::empty(None)
    }
}

fn frame(op: i32, json: &str) -> Vec<u8> {
    let mut buf = op.to_le_bytes().to_vec();
    buf.extend_from_slice(&(json.len() as i32).to_le_bytes());
    buf.extend_from_slice(json.as_bytes());
    buf
}

async fn read_frame(s: &mut DuplexStream) -> (i32, String) {
    let mut head = [0u8; 8];
    s.read_exact(&mut head).await.unwrap();
    let len = i32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.unwrap();
    (
        i32::from_le_bytes(head[..4].try_into().unwrap()),
        String::from_utf8(body).unwrap(),
    )
}

/// The monitor's read loop on one end of a pipe; the test is the other end.
fn pipe(m: &Arc<DiscordMonitor>) -> (DuplexStream, tokio::task::JoinHandle<()>, Stop) {
    let (server, client) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = split(Box::new(client));
    let stop = Stop::default();
    let loop_ = tokio::spawn({
        let (m, stop) = (m.clone(), stop.clone());
        async move { m.read_loop(reader, writer, stop).await }
    });
    (server, loop_, stop)
}

async fn until(condition: impl Fn() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(condition());
}

#[tokio::test]
async fn actions_are_refused_unless_control_is_on() {
    for control in [None, Some(false)] {
        let (_s, m) = monitor(control);
        assert!(m.act("mute", None).await.unwrap().contains("turned off"));
    }
}

#[tokio::test]
async fn actions_need_a_connection_even_with_control_on() {
    let (_s, m) = monitor(Some(true));
    assert_eq!(
        m.act("leave", None).await.as_deref(),
        Some("Discord is not connected")
    );
}

#[tokio::test]
async fn a_widget_on_a_disabled_page_does_not_allow_control() {
    let s = setup(Some(true), false, None, true);
    let m = DiscordMonitor::new(s.store.clone());
    assert!(m.act("mute", None).await.unwrap().contains("turned off"));
    assert!(!control_allowed(&s.store.get()));
}

#[test]
fn control_is_allowed_for_an_enabled_page_with_the_option_on() {
    let (s, _m) = monitor(Some(true));
    assert!(control_allowed(&s.store.get()));
}

#[test]
fn snapshot_starts_disconnected_and_empty() {
    let (_s, m) = monitor(Some(true));
    let snap = m.snapshot();
    assert!(!snap.connected);
    assert!(snap.channels.is_empty());
    assert!(snap.notifications.is_empty());
}

#[test]
fn a_poll_that_started_before_a_button_press_does_not_overwrite_its_state() {
    let (_s, m) = monitor(Some(true));
    m.publish_poll(m.voice_revision(), false, false, polled()); // connected, unmuted
    let rev = m.voice_revision(); // a poll reads mute=false here...
    m.apply_voice(Some(&json!({"mute": true, "deaf": false}))); // ...then a press mutes
    m.publish_poll(rev, false, false, polled()); // ...and the old poll finishes
    assert!(m.snapshot().muted);
    m.publish_poll(m.voice_revision(), true, false, polled()); // the next poll agrees
    assert!(m.snapshot().muted);
}

#[test]
fn a_poll_started_after_the_press_is_trusted() {
    let (_s, m) = monitor(Some(true));
    m.publish_poll(m.voice_revision(), false, false, polled());
    m.apply_voice(Some(&json!({"mute": true})));
    m.publish_poll(m.voice_revision(), false, true, polled()); // the user unmuted in the Discord app itself
    assert!(!m.snapshot().muted);
    assert!(m.snapshot().deafened);
}

#[test]
fn reconnects_back_off_from_5_to_30_seconds() {
    for (failures, seconds) in [(0, 5), (1, 5), (2, 10), (3, 20), (4, 30), (40, 30)] {
        assert_eq!(
            backoff(failures),
            Duration::from_secs(seconds),
            "{failures}"
        );
    }
}

#[test]
fn notifications_are_wanted_only_while_an_enabled_widget_shows_them() {
    // the option defaults to on; a widget on a disabled page does not count
    for (option, page_enabled, expected) in [
        (None, true, true),
        (Some(true), true, true),
        (Some(false), true, false),
        (Some(true), false, false),
    ] {
        let s = setup(Some(false), page_enabled, option, true);
        assert_eq!(
            notifications_wanted(&s.store.get()),
            expected,
            "{option:?} {page_enabled}"
        );
    }
}

#[tokio::test]
async fn a_ping_from_discord_is_answered_with_a_pong_carrying_the_same_payload() {
    let (_s, m) = monitor(Some(false));
    let m = Arc::new(m);
    let (mut server, loop_, stop) = pipe(&m);
    server
        .write_all(&frame(3, r#"{"nonce":"abc"}"#))
        .await
        .unwrap();
    let (op, body) = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut server))
        .await
        .unwrap();
    assert_eq!(op, 4);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["nonce"],
        "abc"
    );
    stop.stop();
    let _ = loop_.await;
}

#[tokio::test]
async fn a_request_still_waiting_ends_with_no_answer_when_discord_closes_the_connection() {
    // the login prompt waits up to two minutes; when Discord quits meanwhile it must end at once as "no answer"
    let (_s, m) = monitor(Some(false));
    let m = Arc::new(m);
    let waiting = m.wait_for("login");
    let (server, loop_, _stop) = pipe(&m);
    drop(server);
    tokio::time::timeout(Duration::from_secs(10), loop_)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(waiting.await.unwrap(), Ok(None));
}

#[tokio::test]
async fn turning_notifications_off_hides_their_text_at_once() {
    let (s, m) = monitor(Some(false));
    let m = Arc::new(m);
    let (mut server, loop_, stop) = pipe(&m);
    m.set_notifications_on(true);
    server
        .write_all(&frame(
            1,
            r#"{"evt":"NOTIFICATION_CREATE","data":{"title":"Ana","body":"secret DM"}}"#,
        ))
        .await
        .unwrap();
    until(|| m.snapshot().notifications.len() == 1).await;
    // the widget's notifications are turned off in the settings; the monitor's own pass never runs here, so the
    // text going away can only come from snapshot() reading the settings itself
    s.store
        .save(
            &config(Some(false), true, Some(false), true).to_string(),
            None,
        )
        .unwrap();
    until(|| m.snapshot().notifications.is_empty()).await;
    stop.stop();
    let _ = loop_.await;
    let _ = &s.path;
}

#[tokio::test]
async fn notification_text_is_dropped_unless_notifications_are_on() {
    let (_s, m) = monitor(Some(false));
    let m = Arc::new(m);
    let (mut server, loop_, stop) = pipe(&m);
    let note = r#"{"evt":"NOTIFICATION_CREATE","data":{"title":"Ana","body":"secret DM"}}"#;
    server.write_all(&frame(1, note)).await.unwrap();
    server.write_all(&frame(3, "{}")).await.unwrap(); // once this is answered, the note before it was handled
    read_frame(&mut server).await;
    m.set_notifications_on(true);
    assert!(m.snapshot().notifications.is_empty()); // dropped, not stored for later
    server.write_all(&frame(1, note)).await.unwrap();
    until(|| m.snapshot().notifications.len() == 1).await;
    let n = &m.snapshot().notifications[0];
    assert_eq!((n.title.as_str(), n.body.as_str()), ("Ana", "secret DM"));
    m.set_notifications_on(false);
    assert!(m.snapshot().notifications.is_empty()); // not sent to the phone while off
    stop.stop();
    let _ = loop_.await;
}

#[tokio::test]
async fn image_requests_with_unsafe_ids_are_rejected_without_a_network_call() {
    for (kind, id, hash) in [
        ("bad", "1", "abc"),      // unknown kind
        ("guild", "../1", "abc"), // id must be digits only
        ("guild", "", "abc"),
        ("guild", "1", "a/b"), // hash must be letters, digits or underscore
        ("user", "1", ""),
        ("user", "123456789012345678901234567890", "abc"),
    ] {
        let (_s, m) = monitor(Some(false));
        assert_eq!(m.image(kind, id, hash).await, None, "{kind} {id} {hash}");
    }
}

fn with_known_images() -> DiscordSnapshot {
    DiscordSnapshot {
        connected: true,
        user: Some("me".into()),
        channel_id: Some("c1".into()),
        channel_name: Some("Room".into()),
        channel_kind: Some("voice".into()),
        members: vec![DiscordMember {
            id: "7".into(),
            name: "Ana".into(),
            muted: false,
            deafened: false,
            avatar: Some("avatar1".into()),
        }],
        channels: vec![DiscordChannel {
            id: "c1".into(),
            name: "Room".into(),
            guild: Some("Server".into()),
            guild_id: Some("42".into()),
            guild_icon: Some("icon1".into()),
        }],
        ..DiscordSnapshot::empty(None)
    }
}

#[tokio::test]
async fn images_are_refused_while_discord_is_off() {
    let s = setup(Some(false), true, None, false);
    let m = DiscordMonitor::new(s.store.clone());
    m.publish_poll(0, false, false, with_known_images());
    assert!(m.is_known_image("guild", "42", "icon1")); // known to the snapshot, but still refused
    assert_eq!(m.image("guild", "42", "icon1").await, None);
}

#[tokio::test]
async fn only_icons_and_avatars_from_the_current_snapshot_are_served() {
    let (_s, m) = monitor(Some(false));
    assert!(!m.is_known_image("guild", "42", "icon1")); // nothing reported yet
    m.publish_poll(0, false, false, with_known_images());
    assert!(m.is_known_image("guild", "42", "icon1"));
    assert!(m.is_known_image("user", "7", "avatar1"));
    assert!(!m.is_known_image("guild", "42", "other")); // right server, a hash the agent did not report
    assert!(!m.is_known_image("guild", "43", "icon1"));
    assert!(!m.is_known_image("user", "42", "icon1")); // a server id is not a user id
    assert_eq!(m.image("guild", "43", "icon1").await, None);
}

#[tokio::test]
async fn token_path_follows_config_directory_and_read_loop_survives_malformed_frames_and_clears_on_close()
 {
    let (s, m) = monitor(Some(false));
    let config_dir = std::path::absolute(&s.path)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(m.token_path(), config_dir.join("discord-token.json"));
    let m = Arc::new(m);
    m.set_notifications_on(true);
    m.publish_poll(m.voice_revision(), false, false, polled());
    let (mut server, loop_, _stop) = pipe(&m);
    server.write_all(&frame(1, "[1,2,3]")).await.unwrap();
    server
        .write_all(&frame(1, r#"{"nonce":123,"evt":456}"#))
        .await
        .unwrap();
    server
        .write_all(&frame(3, r#"{"nonce":"still-alive"}"#))
        .await
        .unwrap();
    let (op, body) = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut server))
        .await
        .unwrap();
    assert_eq!(op, 4);
    assert!(body.contains("still-alive"));
    drop(server);
    tokio::time::timeout(Duration::from_secs(10), loop_)
        .await
        .unwrap()
        .unwrap();
    assert!(!m.snapshot().connected);
    assert_eq!(
        m.snapshot().error.as_deref(),
        Some("Discord is not running")
    );
}

// The last poll reply can arrive just before Discord closes the pipe; the poll that was waiting for it finishes
// after the reader cleared the snapshot and must not mark the closed session connected again.
#[tokio::test]
async fn a_poll_finishing_after_the_connection_closed_does_not_bring_it_back() {
    let (_s, m) = monitor(Some(false));
    let m = Arc::new(m);
    let (server, loop_, _stop) = pipe(&m);
    tokio::task::yield_now().await;
    until(|| !lock(&m.shared).closed).await;
    m.publish_poll(m.voice_revision(), false, false, polled());
    assert!(m.snapshot().connected);
    let rev = m.voice_revision(); // a poll reads its answers here...
    drop(server); // ...Discord closes the pipe...
    tokio::time::timeout(Duration::from_secs(10), loop_)
        .await
        .unwrap()
        .unwrap();
    m.publish_poll(rev, false, false, polled()); // ...and the poll publishes what it read
    assert!(!m.snapshot().connected);
}

// --config with a settings file named like the login file: saving a login must not replace the settings.
#[test]
fn the_login_file_never_is_the_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("discord-token.json");
    std::fs::write(&settings, "{}").unwrap();
    let m = DiscordMonitor::new(Arc::new(ConfigStore::new(settings.clone())));
    let token = m.token_path();
    assert!(
        !token
            .to_string_lossy()
            .eq_ignore_ascii_case(&std::path::absolute(&settings).unwrap().to_string_lossy())
    );
    assert_eq!(
        token.parent().unwrap(),
        std::path::absolute(dir.path()).unwrap()
    );
}

#[test]
fn the_saved_login_is_read_and_written_as_the_csharp_agent_does() {
    let t: Tokens =
        serde_json::from_str(r#"{"ClientId":"1","Access":"a","Refresh":"r","Expires":1794045600}"#)
            .unwrap();
    assert_eq!(
        (t.client_id.as_str(), t.access.as_str(), t.expires),
        ("1", "a", 1_794_045_600)
    );
    assert_eq!(
        serde_json::to_string(&t).unwrap(),
        r#"{"ClientId":"1","Access":"a","Refresh":"r","Expires":1794045600}"#
    );
}
