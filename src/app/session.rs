/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Login, SSO, session restore, logout and device verification.

use crate::app::decode::load_all_packs;
use crate::app::sync::sync_filter;
use crate::app::text::short_room;
use crate::app::{
    LoggedIn, LoginMsg, RoomEntry, RoomInfo, RoomMeta, RoomNotify, SendResult, ThraceApp,
};

/// Blocking password login on a worker thread; saves the session for cached restore.
/// Fresh logins mint a new device id, so the sqlite crypto store must be fresh too.
pub(in crate::app) fn login_blocking(
    homeserver: String,
    username: String,
    password: String,
) -> Result<LoggedIn, String> {
    use matrix_sdk::Client;
    // Fresh login: wipe any prior-device store so ids never collide.
    let store_dir = password_store_dir(&username);
    let _ = std::fs::remove_file(&store_dir);
    let _ = std::fs::remove_dir_all(&store_dir);
    std::fs::create_dir_all(store_dir.parent().unwrap()).map_err(|e| format!("db dir: {e}"))?;
    let store_dir_s = store_dir.to_string_lossy().into_owned();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("rt: {e}"))?;
    rt.block_on(async move {
        let client = Client::builder()
            .homeserver_url(&homeserver)
            .sqlite_store(&store_dir, None)
            .build()
            .await
            .map_err(|e| format!("connect {homeserver}: {e}"))?;
        client
            .matrix_auth()
            .login_username(&username, &password)
            .send()
            .await
            .map_err(|e| format!("login as {username}: {e}"))?;
        finish_login(client, &homeserver, &store_dir_s).await
    })
}

/// Collect the room list from synced state. History loads after the UI is ready.
pub(in crate::app) async fn collect_logged_in(
    client: matrix_sdk::Client,
    _homeserver: &str,
) -> Result<LoggedIn, String> {
    // Filter the initial sync too, or it downloads full member state first.
    client
        .sync_once(matrix_sdk::config::SyncSettings::default().filter(sync_filter()))
        .await
        .map_err(|e| format!("sync: {e}"))?;
    let user_id = client
        .user_id()
        .map(|u| u.to_string())
        .unwrap_or("me".into());
    let emoji_usage = crate::recent_emoji::RecentEmoji::load(&client).await?;
    let packs = load_all_packs(&client).await;
    let mut rooms_info = Vec::new();
    // Read room metadata concurrently from the local SDK store.
    const ROOM_CONCURRENCY: usize = 8;
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(ROOM_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for room in client.joined_rooms() {
        let permits = permits.clone();
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await.ok()?;
            let room_id = room.room_id().to_string();
            // Real name, not room id; falls back to DM peer.
            let name = match room.display_name().await {
                Ok(d) => d.to_string(),
                Err(_) => room.name().unwrap_or_else(|| short_room(&room_id)),
            };
            let name = if name.trim().is_empty() || name == "Empty room" {
                short_room(&room_id)
            } else {
                name
            };
            let is_dm = room.is_direct().await.unwrap_or_else(|_| room.is_dm());
            let avatar_mxc = room.avatar_url().map(|u| u.to_string());
            let last_activity = room
                .latest_event_timestamp()
                .map(|ts| u64::from(ts.get()))
                .unwrap_or_default();
            Some(RoomInfo {
                room_id,
                name,
                is_dm,
                avatar_mxc,
                meta: room_meta(&room),
                last_activity,
                fully_read: fully_read_marker(&room).await,
                notify: room.notification_mode().await.map(RoomNotify::from_sdk),
            })
        });
    }
    while let Some(done) = tasks.join_next().await {
        if let Ok(Some(info)) = done {
            rooms_info.push(info);
        }
    }

    Ok(LoggedIn {
        client,
        user_id,
        rooms: rooms_info,
        packs,
        emoji_usage,
        session_metadata: None,
        persistence_warning: None,
    })
}

/// Topic and tags from the SDK's synced room state.
pub(in crate::app) fn room_meta(room: &matrix_sdk::Room) -> RoomMeta {
    RoomMeta {
        favourite: room.is_favourite(),
        low_priority: room.is_low_priority(),
        topic: room.topic().filter(|topic| !topic.trim().is_empty()),
    }
}

/// The room's `m.fully_read` event, if the account has one stored.
pub(in crate::app) async fn fully_read_marker(room: &matrix_sdk::Room) -> Option<String> {
    use matrix_sdk::ruma::events::fully_read::FullyReadEventContent;
    let raw = room
        .account_data_static::<FullyReadEventContent>()
        .await
        .ok()??;
    Some(raw.deserialize().ok()?.content.event_id.to_string())
}

/// Non-secret login metadata. Tokens live in Secret Service; the store directory
/// must stay paired with the same Matrix device when restoring a session.
pub(in crate::app) fn session_path() -> std::path::PathBuf {
    dirs_data_dir().join("thrace").join("session.json")
}

/// A missing wallet permits an in-memory login, with a visible persistence warning.
pub(in crate::app) async fn finish_login(
    client: matrix_sdk::Client,
    homeserver: &str,
    store_dir: &str,
) -> Result<LoggedIn, String> {
    let session = client.matrix_auth().session().ok_or("No login session")?;
    let metadata = crate::session_store::SessionMetadata::new(homeserver, store_dir, &session);
    let saved = crate::session_store::save(&session_path(), &metadata, &session).await;
    let mut logged = collect_logged_in(client, homeserver).await?;
    logged.session_metadata = Some(metadata);
    logged.persistence_warning = saved
        .err()
        .map(|error| format!("Login is not saved: {error:#}"));
    Ok(logged)
}

/// Per-user password store dir (fresh device per login; wiped above).
pub(in crate::app) fn password_store_dir(username: &str) -> std::path::PathBuf {
    let safe_user: String = username
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    dirs_data_dir()
        .join("thrace")
        .join(format!("{safe_user}.sqlite"))
}

/// Restore the same device without deleting its saved session on transient errors.
pub(in crate::app) fn restore_blocking() -> Result<LoggedIn, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("rt: {e}"))?;
    rt.block_on(async move {
        use matrix_sdk::{AuthSession, Client};
        let (metadata, session) = crate::session_store::load(&session_path())
            .await
            .map_err(|e| format!("restore: {e:#}"))?
            .ok_or("No saved session")?;
        let client = Client::builder()
            .homeserver_url(&metadata.homeserver)
            .sqlite_store(&metadata.store_dir, None)
            .build()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        client
            .restore_session(AuthSession::Matrix(session))
            .await
            .map_err(|e| format!("restore: {e}"))?;
        let mut logged = collect_logged_in(client, &metadata.homeserver).await?;
        logged.session_metadata = Some(metadata);
        Ok(logged)
    })
}

/// One-click SSO/PocketID flow on a worker thread: bind 127.0.0.1:8008, open
/// the URL in the browser, wait for the loopback token, finish login + sync.
pub(in crate::app) fn sso_auto_flow(homeserver: String, tx: std::sync::mpsc::Sender<LoginMsg>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:8008").and_then(|l| {
        l.set_nonblocking(false)?;
        Ok(l)
    });
    let listener = match listener {
        Ok(l) => l,
        Err(e) => {
            let _ = tx.send(LoginMsg::SsoDone(Err(format!(
                "loopback :8008 busy: {e} — use token paste"
            ))));
            return;
        }
    };
    if let Err(e) = sso_build_and_wait(homeserver, listener, tx.clone()) {
        let _ = tx.send(LoginMsg::SsoDone(Err(e)));
    }
}

/// Build the SSO client + URL. Each attempt gets a unique store dir: SSO mints
/// a new device id, and a reused store fails the account-match check.
pub(in crate::app) fn sso_build_only(
    homeserver: &str,
) -> Result<(matrix_sdk::Client, String, String), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("rt: {e}"))?;
    rt.block_on(async {
        use matrix_sdk::Client;
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir_name = format!("sso-{}-{}.sqlite", millis, std::process::id());
        let db_dir = dirs_data_dir().join("thrace").join(dir_name);
        // Drop the legacy shared store so it can never collide again.
        let _ = std::fs::remove_file(dirs_data_dir().join("thrace").join("sso.sqlite"));
        let _ = std::fs::remove_dir_all(dirs_data_dir().join("thrace").join("sso.sqlite"));
        let store_dir_s = db_dir.to_string_lossy().into_owned();
        let client = Client::builder()
            .homeserver_url(homeserver)
            .sqlite_store(&db_dir, None)
            .build()
            .await
            .map_err(|e| format!("connect {homeserver}: {e}"))?;
        let url = client
            .matrix_auth()
            .get_sso_login_url("http://localhost:8008/callback", None)
            .await
            .map_err(|e| format!("sso url: {e}"))?;
        Ok((client, url.to_string(), store_dir_s))
    })
}

/// Build client, send URL, wait for the loopback token, finish. `Err` only
/// when Done already went out via tx.
pub(in crate::app) fn sso_build_and_wait(
    homeserver: String,
    listener: std::net::TcpListener,
    tx: std::sync::mpsc::Sender<LoginMsg>,
) -> Result<(), String> {
    let (client, url, store_dir) = sso_build_only(&homeserver)?;
    let _ = tx.send(LoginMsg::SsoUrlReady(Ok((
        client.clone(),
        url,
        store_dir.clone(),
    ))));
    // Block for the PocketID → Synapse → loopback redirect (worker thread only).
    let token = wait_for_loopback_token(listener)?;
    let res = sso_finish_blocking(client, store_dir, token);
    let _ = tx.send(LoginMsg::SsoDone(res));
    Ok(())
}

/// Minimal loopback HTTP: one GET, parse loginToken, reply 200, return it.
pub(in crate::app) fn wait_for_loopback_token(
    listener: std::net::TcpListener,
) -> Result<String, String> {
    use std::io::{Read, Write};
    for stream in listener.incoming() {
        let mut stream = stream.map_err(|e| format!("loopback accept: {e}"))?;
        let mut buf = [0u8; 8192];
        let n = stream
            .read(&mut buf)
            .map_err(|e| format!("loopback read: {e}"))?;
        let req = String::from_utf8_lossy(&buf[..n]).into_owned();
        let body = "<html><body><h2>Logged in — you can close this tab</h2></body></html>";
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let _ = stream.write_all(resp.as_bytes());
        let line = req.lines().next().unwrap_or("");
        if let Some(token) = extract_token_from_get(line) {
            if !token.is_empty() {
                return Ok(token);
            }
        }
        // Ignore favicon etc; keep waiting.
    }
    Err("SSO timed out waiting on localhost:8008".into())
}

pub(in crate::app) fn extract_token_from_get(request_line: &str) -> Option<String> {
    // "GET /callback?loginToken=ABC&x=1 HTTP/1.1"; reuse the query parser.
    let path = request_line.split_whitespace().nth(1)?;
    let fake = format!("http://localhost{path}");
    let parsed = url::Url::parse(&fake).ok()?;
    for (k, v) in parsed.query_pairs() {
        if k == "loginToken" {
            return Some(v.into_owned());
        }
    }
    None
}

/// Finish SSO with a loginToken (worker thread). `store_dir` stays the unique
/// dir from the pre-login client.
pub(in crate::app) fn sso_finish_blocking(
    client: matrix_sdk::Client,
    store_dir: String,
    token_or_url: String,
) -> Result<LoggedIn, String> {
    // Homeserver comes from the client, for session persist.
    let hs = client.homeserver().to_string();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("rt: {e}"))?;
    rt.block_on(async move {
        let token = extract_login_token(&token_or_url);
        client
            .matrix_auth()
            .login_token(&token)
            .send()
            .await
            .map_err(|e| format!("sso login: {e}"))?;
        finish_login(client, &hs, &store_dir).await
    })
}

/// Accept a raw loginToken or a full `...?loginToken=XYZ` callback URL.
pub(in crate::app) fn extract_login_token(s: &str) -> String {
    let t = s.trim();
    if let Some(i) = t.find("loginToken=") {
        let rest = &t[i + "loginToken=".len()..];
        let end = rest.find('&').unwrap_or(rest.len());
        return rest[..end].to_owned();
    }
    t.to_owned()
}

pub(in crate::app) fn dirs_data_dir() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        std::path::PathBuf::from(home).join(".local/share")
    } else {
        std::env::temp_dir()
    }
}

impl ThraceApp {
    // Login: password + SSO/OIDC.
    pub(in crate::app) fn start_login(&mut self) {
        if self.login.busy {
            return;
        }
        let hs = self.login.homeserver.trim().to_owned();
        let user = self.login.username.trim().to_owned();
        let pass = self.login.password.clone();
        if hs.is_empty() || user.is_empty() || pass.is_empty() {
            self.login.error = "fill homeserver + username + password, or use SSO".into();
            return;
        }
        self.login.busy = true;
        self.login.error.clear();
        self.status = format!("connecting to {hs} as {user} …");
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);
        self.spawn_blocking_task(move || {
            let res = login_blocking(hs, user, pass);
            let _ = tx.send(LoginMsg::PasswordDone(res));
        });
    }

    pub(in crate::app) fn start_sso(&mut self, _ctx: &egui::Context) {
        if self.login.busy {
            return;
        }
        let hs = self.login.homeserver.trim().to_owned();
        if hs.is_empty() {
            self.login.error = "set homeserver first".into();
            return;
        }
        self.login.busy = true;
        self.login.error.clear();
        self.login.show_sso_token = false;
        self.status = format!("SSO (PocketID) on {hs} — opening browser …");
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);
        // One-shot auto flow: worker binds loopback, finishes login, sends Done.
        self.spawn_blocking_task(move || {
            sso_auto_flow(hs, tx);
        });
    }

    pub(in crate::app) fn poll_login(&mut self, ctx: &egui::Context) {
        // Drain pending; auto flow sends UrlReady then Done.
        loop {
            let msg: Option<LoginMsg> = match &self.login_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(msg) = msg else { break };
            match msg {
                LoginMsg::PasswordDone(res) => {
                    self.login.busy = false;
                    self.login_rx = None;
                    match res {
                        Ok(logged) => self.apply_logged_in(logged),
                        Err(e) => {
                            self.login.error = e.clone();
                            self.status = format!("login failed: {e}");
                        }
                    }
                }
                LoginMsg::SsoUrlReady(res) => {
                    match res {
                        Ok((client, url, store_dir)) => {
                            self.pending_sso_client = Some((client, store_dir));
                            self.login.sso_url = url.clone();
                            self.status = "SSO — finish in browser, auto-completing …".into();
                            ctx.open_url(egui::OpenUrl { url, new_tab: true });
                            // Keep rx open; Done follows.
                        }
                        Err(e) => {
                            self.login.busy = false;
                            self.login_rx = None;
                            self.login.error = e.clone();
                            self.status = format!("SSO failed: {e}");
                        }
                    }
                }
                LoginMsg::SsoDone(res) => {
                    self.login.busy = false;
                    self.login_rx = None;
                    match res {
                        Ok(logged) => {
                            self.pending_sso_client = None;
                            self.apply_logged_in(logged);
                        }
                        Err(e) => {
                            self.login.error = e.clone();
                            self.status = format!("SSO failed: {e} — token paste fallback below");
                            self.login.show_sso_token = true;
                        }
                    }
                }
            }
        }
    }

    pub(in crate::app) fn finish_sso_manual(&mut self) {
        // Fallback: paste token when loopback is blocked.
        let Some((client, store_dir)) = self.pending_sso_client.clone() else {
            self.login.error = "hit [sso] first".into();
            return;
        };
        let token = extract_login_token(&self.login.sso_token_input);
        if token.is_empty() {
            self.login.error = "paste loginToken or full callback URL".into();
            return;
        }
        self.login.busy = true;
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);
        self.spawn_blocking_task(move || {
            let res = sso_finish_blocking(client, store_dir, token);
            let _ = tx.send(LoginMsg::SsoDone(res));
        });
    }

    /// Logout: stop sync, drop client, wipe session cache.
    pub(in crate::app) fn logout(&mut self) {
        if let Some(task) = self.session_save_task.take() {
            task.abort();
        }
        self.session_save_rx = None;
        self.session_warning = None;
        let client = self.client.clone();
        let metadata = self.session_metadata.take();
        let forget_error = crate::session_store::forget_file(&session_path()).err();
        self.stop_live_sync();
        self.stop_history_loading();
        self.stop_emoji_saving();
        self.stop_media_worker();
        self.audio = None;
        self.last_receipt.clear();
        self.relations_fetched.clear();
        self.relations_busy = false;
        self.react_rx = None;
        self.react_tx = None;
        self.own_user.clear();
        self.verify_watch_running = false;
        self.watch_rx = None;
        self.client = None;
        std::rc::Rc::make_mut(&mut self.rows).clear();
        self.members.clear();
        self.login.password.clear();
        self.login.busy = false;
        self.login.error.clear();
        self.show_security = false;
        self.devices.clear();
        self.sas = None;
        self.pending_flow = None;
        self.incoming = None;
        self.status = "logged out — log in to sync".into();
        if let Some(error) = forget_error {
            self.status = format!("Logged out, but could not remove saved metadata: {error:#}");
        }
        self.spawn_send(async move {
            // Wallet cleanup must not wait for a slow or unreachable homeserver.
            let (server, wallet) = tokio::join!(
                async {
                    if let Some(client) = client {
                        tokio::time::timeout(std::time::Duration::from_secs(30), client.logout())
                            .await
                            .map_err(|_| "server logout timed out".to_owned())?
                            .map_err(|error| format!("server logout: {error}"))?;
                    }
                    Ok::<(), String>(())
                },
                async {
                    if let Some(metadata) = metadata {
                        crate::session_store::delete_tokens(&metadata)
                            .await
                            .map_err(|error| format!("wallet cleanup: {error:#}"))?;
                    }
                    Ok::<(), String>(())
                },
            );
            let errors: Vec<_> = [server, wallet]
                .into_iter()
                .filter_map(Result::err)
                .collect();
            if errors.is_empty() {
                SendResult::Done(String::new())
            } else {
                SendResult::Failed(errors.join("; "))
            }
        });
    }

    pub(in crate::app) fn apply_logged_in(&mut self, logged: LoggedIn) {
        self.stop_history_loading();
        self.stop_emoji_saving();
        self.status = logged
            .persistence_warning
            .clone()
            .unwrap_or_else(|| format!("{} — {} rooms", logged.user_id, logged.rooms.len()));
        self.session_metadata = logged.session_metadata;
        self.session_warning = logged.persistence_warning;
        self.login.homeserver = logged.client.homeserver().to_string();
        self.own_user = logged.user_id.clone();
        self.emoji_usage = logged.emoji_usage;
        self.recent_emoji = self.emoji_usage.frequent(18);
        // Display names + DM flag; sort rooms then DMs.
        let mut rooms: Vec<RoomEntry> = logged
            .rooms
            .iter()
            .map(|r| RoomEntry {
                room_id: r.room_id.clone(),
                name: if r.name.is_empty() {
                    short_room(&r.room_id)
                } else {
                    r.name.clone()
                },
                unread: 0,
                mentioned: false,
                is_dm: r.is_dm,
                avatar_mxc: r.avatar_mxc.clone(),
                preview: None,
                meta: r.meta.clone(),
                last_activity: r.last_activity,
                fully_read: r.fully_read.clone(),
                notify: r.notify,
            })
            .collect();
        rooms.sort_by(|a, b| {
            a.is_dm
                .cmp(&b.is_dm)
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        if rooms.is_empty() {
            self.status = "no rooms yet — join one with /join #alias:hs".into();
        }
        // Unread counts start at zero, so the first room opens without a "New messages" line.
        self.unread_marker = None;
        self.rooms = rooms;
        self.current = 0;
        self.timelines.clear();
        self.back_tokens.clear();
        self.members_by_room.clear();
        self.members.clear();
        self.rows = Default::default();
        self.history_queue = crate::history_queue::HistoryQueue::new(
            self.rooms.iter().map(|room| room.room_id.clone()),
        );
        // Merge real packs after demo pack (first-wins).
        for pack in logged.packs.packs().iter().cloned() {
            self.packs.upsert_pack(pack);
        }
        // Prime media queue with custom emoji so picker/chat don't stall.
        for pack in self.packs.packs() {
            for img in &pack.images {
                self.media
                    .texture_for("emoji", &img.mxc_url, Some((24, 24)));
            }
        }
        self.client = Some(logged.client);
        // Start the visible room immediately. Frontends without egui repaint
        // callbacks (the Slint bridge) must not depend on a later frame to
        // begin initial history after a logout/login cycle.
        if let Some(room_id) = self.current_room_id() {
            self.history_queue.select(&room_id);
        }
        self.pump_history();
        self.refresh_devices();
        // Fresh login: reset watcher + de-dupe set.
        self.verify_watch_running = false;
        self.watch_rx = None;
        self.seen_incoming.clear();
        self.incoming = None;
        // Fresh login: restart live-sync loop.
        self.stop_live_sync();
        // Media worker holds the old client; restart it too.
        self.stop_media_worker();
        self.last_receipt.clear();
    }

    // Verification (SAS).
    pub(in crate::app) fn refresh_devices(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let user = if self.verify_user_input.trim().is_empty() {
            match client.user_id() {
                Some(u) => u.to_owned(),
                None => return,
            }
        } else {
            match matrix_sdk::ruma::OwnedUserId::try_from(self.verify_user_input.trim()) {
                Ok(u) => u,
                Err(_) => {
                    self.status = "bad @user:hs".into();
                    return;
                }
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.verify_rx = Some(rx);
        self.status = format!("loading devices for {user} …");
        self.spawn_task(async move {
            match crate::verify::list_devices(&client, &user).await {
                Ok(devs) => {
                    let _ = tx.send(crate::verify::VerifyEvent::Devices(devs));
                }
                Err(e) => {
                    let _ = tx.send(crate::verify::VerifyEvent::Error(e));
                }
            }
        });
    }

    pub(in crate::app) fn start_verify_device(&mut self, user_id: &str, device_id: &str) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.verify_rx = Some(rx);
        self.sas = None;
        self.pending_flow = None;
        self.status =
            format!("requesting verification {user_id}:{device_id} … accept on other device");
        let user: matrix_sdk::ruma::OwnedUserId = match user_id.try_into() {
            Ok(u) => u,
            Err(_) => {
                self.status = "bad user id".into();
                return;
            }
        };
        let dev: matrix_sdk::ruma::OwnedDeviceId = device_id.into();
        self.spawn_task(async move {
            if let Err(e) =
                crate::verify::outgoing_verify_to_device(client, &user, &dev, tx.clone()).await
            {
                // `tx` is the live one-shot channel; errors must surface in `poll_verify`.
                let _ = tx.send(crate::verify::VerifyEvent::Error(e));
            }
        });
    }

    /// Background watcher: poll for incoming verification requests every 5s.
    pub(in crate::app) fn ensure_verify_watch(&mut self, ctx: &egui::Context) {
        if self.verify_watch_running || self.client.is_none() {
            return;
        }
        self.verify_watch_running = true;
        let client = self.client.clone().unwrap();
        // Dedicated watch_rx coexists with one-shot verify_rx flows.
        let (tx, rx) = std::sync::mpsc::channel();
        self.watch_rx = Some(rx);
        let ctx = ctx.clone();
        self.spawn_task(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                // Client replaced: exit; fresh watcher starts.
                if client.user_id().is_none() {
                    break;
                }
                match crate::verify::poll_incoming_request(&client).await {
                    Some(req) => {
                        let _ = tx.send(crate::verify::IncomingRequest {
                            flow_id: req.flow_id,
                            user_id: req.user_id,
                            device_id: req.device_id,
                        });
                        ctx.request_repaint();
                    }
                    None => continue,
                }
            }
        });
    }

    pub(in crate::app) fn accept_incoming(&mut self) {
        let Some(req) = self.incoming.clone() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        let Ok(user) = matrix_sdk::ruma::OwnedUserId::try_from(req.user_id.clone()) else {
            self.status = "bad user id in verification request".into();
            return;
        };
        self.incoming = None;
        self.show_security = true;
        self.sas = None;
        self.pending_flow = None;
        self.status = format!("accepting verification from {} …", req.user_id);
        let (tx, rx) = std::sync::mpsc::channel();
        self.verify_rx = Some(rx);
        let flow = req.flow_id.clone();
        self.spawn_task(async move {
            if let Err(e) =
                crate::verify::accept_incoming_request(client, &user, &flow, tx.clone()).await
            {
                let _ = tx.send(crate::verify::VerifyEvent::Error(e));
            }
        });
    }

    pub(in crate::app) fn decline_incoming(&mut self) {
        let Some(req) = self.incoming.take() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        self.status = format!("declined verification from {}", req.user_id);
        let flow = req.flow_id.clone();
        let user_s = req.user_id.clone();
        self.spawn_task(async move {
            if let Ok(user) = matrix_sdk::ruma::OwnedUserId::try_from(user_s) {
                let _ = crate::verify::decline_incoming_request(&client, &user, &flow).await;
            }
        });
    }

    pub(in crate::app) fn poll_verify(&mut self) {
        // Watcher speaks IncomingRequest on its own channel.
        loop {
            let req = match &self.watch_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(req) = req else { break };
            // De-dupe re-scanned flows.
            if self.seen_incoming.insert(req.flow_id.clone()) {
                self.incoming = Some(req);
                self.status = "incoming verification — accept?".into();
            }
        }
        // Drain pending; one-shot flows share verify_rx.
        loop {
            let msg = match &self.verify_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(msg) = msg else { break };
            match msg {
                crate::verify::VerifyEvent::Devices(devs) => {
                    self.devices = devs;
                    self.status = format!("{} device(s)", self.devices.len());
                }
                crate::verify::VerifyEvent::SasReady { flow_id, sas } => {
                    self.pending_flow = Some(flow_id);
                    self.sas = Some(sas);
                    self.show_security = true;
                    self.status = "SAS ready — compare emojis on BOTH devices".into();
                }
                crate::verify::VerifyEvent::SasDone { .. } => {
                    self.sas = None;
                    self.pending_flow = None;
                    self.status = "verification complete — shields green".into();
                    self.refresh_devices();
                }
                crate::verify::VerifyEvent::Incoming(req) => {
                    // Legacy path; kept for forward-compat.
                    if self.seen_incoming.insert(req.flow_id.clone()) {
                        self.incoming = Some(req);
                        self.status = "incoming verification — accept?".into();
                    }
                }
                crate::verify::VerifyEvent::IncomingAccepted { flow_id } => {
                    self.status = format!("accepted {flow_id} — waiting for SAS …");
                }
                crate::verify::VerifyEvent::Error(e) => {
                    self.status = format!("verify: {e}");
                }
            }
        }
    }

    pub(in crate::app) fn verify_confirm(&mut self, matched: bool) {
        let Some(flow) = self.pending_flow.clone() else {
            return;
        };
        let Some(sas) = crate::verify::take_pending_sas(&flow) else {
            self.status = "SAS expired — restart verify".into();
            return;
        };
        self.status = if matched {
            "confirming match …".into()
        } else {
            "reporting mismatch …".into()
        };
        self.spawn_task(async move {
            let _ = if matched {
                sas.confirm().await
            } else {
                sas.mismatch().await
            };
        });
        self.sas = None;
        self.pending_flow = None;
    }
}
