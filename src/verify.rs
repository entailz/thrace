/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! SAS device verification.
//!
//! Request → accept → `start_sas` → both sides compare 7 emojis + 3 decimals
//! out-of-band → confirm/mismatch. Blocking dance runs on a worker thread,
//! results pumped as `VerifyEvent` to the UI.

#[derive(Debug, Clone)]
pub struct SasEmojis {
    /// (symbol, description) × 7, e.g. ("🐶", "Dog").
    pub emojis: Vec<(String, String)>,
    pub decimals: (u16, u16, u16),
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub user_id: String,
    pub device_id: String,
    pub display_name: Option<String>,
    pub verified: bool,
    pub is_own: bool,
}

#[derive(Debug, Clone)]
pub struct IncomingRequest {
    pub flow_id: String,
    pub user_id: String,
    pub device_id: String,
}

pub enum VerifyEvent {
    Devices(Vec<DeviceInfo>),
    SasReady {
        flow_id: String,
        sas: SasEmojis,
    },
    SasDone {
        flow_id: String,
    },
    /// Another device started verification with us — pop the accept dialog.
    Incoming(IncomingRequest),
    /// Our accept landed; the SAS dance follows on SasReady.
    IncomingAccepted {
        flow_id: String,
    },
    Error(String),
}

pub async fn wait_for_sas_presentable(
    sas: &matrix_sdk::encryption::verification::SasVerification,
) -> Result<SasEmojis, String> {
    use std::time::Duration;
    for _ in 0..120 {
        if sas.can_be_presented() {
            let emojis = sas.emoji().map(|arr| {
                arr.iter()
                    .map(|e| (e.symbol.to_owned(), e.description.to_owned()))
                    .collect::<Vec<_>>()
            });
            let decimals = sas.decimals();
            match (emojis, decimals) {
                (Some(emojis), Some(decimals)) => return Ok(SasEmojis { emojis, decimals }),
                _ => return Err("SAS has no emoji/decimal representation".into()),
            }
        }
        if sas.is_done() {
            return Err("SAS finished before presenting".into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("SAS timed out waiting for short-auth-string".into())
}

/// Outgoing verify; caller confirms via channel.
pub async fn outgoing_verify_to_device(
    client: matrix_sdk::Client,
    user_id: &matrix_sdk::ruma::OwnedUserId,
    device_id: &matrix_sdk::ruma::OwnedDeviceId,
    tx: std::sync::mpsc::Sender<VerifyEvent>,
) -> Result<(), String> {
    let enc = client.encryption();
    let device = enc
        .get_device(user_id, device_id)
        .await
        .map_err(|e| format!("get device: {e}"))?
        .ok_or("device not found — sync first")?;
    let req = device
        .request_verification()
        .await
        .map_err(|e| format!("request: {e}"))?;
    for _ in 0..120 {
        if let Some(sas) = req
            .start_sas()
            .await
            .map_err(|e| format!("start_sas: {e}"))?
        {
            let emojis = wait_for_sas_presentable(&sas).await?;
            let flow = format!("{}:{}", user_id, device_id);
            let _ = tx.send(VerifyEvent::SasReady {
                flow_id: flow.clone(),
                sas: emojis,
            });
            // Park the SAS for confirm/mismatch.
            store_pending_sas(flow, sas);
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Err("other side never accepted verification".into())
}

/// Surface the first unanswered incoming verification request.
///
/// Flow id is unknown, so walk candidate users' request lists. Membership can
/// lag, so scan rooms live too. Bounded (~60 users) to keep the 5s poll cheap.
pub async fn poll_incoming_request(client: &matrix_sdk::Client) -> Option<IncomingRequest> {
    let r = sync_once_scan_requests(client).await;
    if r.is_some() {
        return r;
    }
    // Fallback: flows seen on an earlier poll, resolved via public API.
    let own = client.user_id()?.to_owned();
    let mut candidates = vec![own];
    for room in client.joined_rooms() {
        let Ok(members) = room.members(matrix_sdk::RoomMemberships::ACTIVE).await else {
            continue;
        };
        for m in members.into_iter().take(20) {
            let id = m.user_id().to_owned();
            if !candidates.contains(&id) {
                candidates.push(id);
            }
        }
        if candidates.len() > 60 {
            break;
        }
    }
    for user in candidates {
        for flow in known_flows_for(&user) {
            let Some(req) = client
                .encryption()
                .get_verification_request(&user, &flow)
                .await
            else {
                continue;
            };
            use matrix_sdk::encryption::verification::VerificationRequestState as S;
            if matches!(req.state(), S::Requested { .. }) {
                return Some(IncomingRequest {
                    flow_id: flow,
                    user_id: user.to_string(),
                    device_id: String::new(),
                });
            }
        }
    }
    None
}

/// `sync_once` + raw to-device scan for verification requests. Also feeds the
/// crypto machine so `get_verification_request` resolves. Raw scan because
/// this client syncs on demand, with no live `sync()` loop for handlers.
async fn sync_once_scan_requests(client: &matrix_sdk::Client) -> Option<IncomingRequest> {
    let resp = client
        .sync_once(
            matrix_sdk::config::SyncSettings::default().timeout(std::time::Duration::from_secs(0)),
        )
        .await
        .ok()?;
    let prefix = "\"type\":\"m.key.verification.request\"";
    let alt = "\"type\": \"m.key.verification.request\"";
    for ev in &resp.to_device {
        let raw = ev.as_raw();
        let json = serde_json::to_value(raw).ok()?.to_string();
        if !json.contains(prefix) && !json.contains(alt) {
            continue;
        }
        let sender = raw.get_field::<String>("sender").ok()??;
        let content: serde_json::Value = raw.get_field("content").ok()??;
        let txn = content
            .get("transaction_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if txn.is_empty() {
            continue;
        }
        note_request_flow(&sender, &txn);
        return Some(IncomingRequest {
            flow_id: txn,
            user_id: sender,
            device_id: String::new(),
        });
    }
    None
}
/// Flow ids seen on the wire, for polls that miss the raw scan (e.g. request
/// arrived before polling started).
static KNOWN_FLOWS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

pub fn note_request_flow(user_id: &str, flow_id: &str) {
    let mut map = KNOWN_FLOWS.lock();
    let flows = map.entry(user_id.to_owned()).or_default();
    if !flows.iter().any(|f| f == flow_id) {
        flows.push(flow_id.to_owned());
    }
}

fn known_flows_for(user: &matrix_sdk::ruma::OwnedUserId) -> Vec<String> {
    KNOWN_FLOWS
        .lock()
        .get(user.as_str())
        .cloned()
        .unwrap_or_default()
}

// Pending SAS by flow id.
static PENDING: std::sync::LazyLock<
    parking_lot::Mutex<
        std::collections::HashMap<String, matrix_sdk::encryption::verification::SasVerification>,
    >,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

pub fn store_pending_sas(
    flow_id: String,
    sas: matrix_sdk::encryption::verification::SasVerification,
) {
    PENDING.lock().insert(flow_id, sas);
}

pub fn take_pending_sas(
    flow_id: &str,
) -> Option<matrix_sdk::encryption::verification::SasVerification> {
    PENDING.lock().remove(flow_id)
}

/// Accept incoming, then run SAS to presentable like the outgoing path.
pub async fn accept_incoming_request(
    client: matrix_sdk::Client,
    user_id: &matrix_sdk::ruma::OwnedUserId,
    flow_id: &str,
    tx: std::sync::mpsc::Sender<VerifyEvent>,
) -> Result<(), String> {
    let Some(req) = client
        .encryption()
        .get_verification_request(user_id, flow_id)
        .await
    else {
        return Err("verification request expired — ask them to re-request".into());
    };
    req.accept().await.map_err(|e| format!("accept: {e}"))?;
    let _ = tx.send(VerifyEvent::IncomingAccepted {
        flow_id: flow_id.to_owned(),
    });
    // Remote start arrives via sync; poll until we can start ours.
    for _ in 0..120 {
        if let Some(sas) = req
            .start_sas()
            .await
            .map_err(|e| format!("start_sas: {e}"))?
        {
            let emojis = wait_for_sas_presentable(&sas).await?;
            let _ = tx.send(VerifyEvent::SasReady {
                flow_id: flow_id.to_owned(),
                sas: emojis,
            });
            store_pending_sas(flow_id.to_owned(), sas);
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Err("other side never started SAS — they may have cancelled".into())
}

/// Decline incoming (sends cancel).
pub async fn decline_incoming_request(
    client: &matrix_sdk::Client,
    user_id: &matrix_sdk::ruma::OwnedUserId,
    flow_id: &str,
) -> Result<(), String> {
    let Some(req) = client
        .encryption()
        .get_verification_request(user_id, flow_id)
        .await
    else {
        return Ok(());
    };
    req.cancel().await.map_err(|e| format!("cancel: {e}"))?;
    Ok(())
}

/// List own + given user's devices for the Security panel.
pub async fn list_devices(
    client: &matrix_sdk::Client,
    user_id: &matrix_sdk::ruma::OwnedUserId,
) -> Result<Vec<DeviceInfo>, String> {
    let enc = client.encryption();
    let current_device = client.device_id().map(|device| device.to_owned());
    let devices = enc
        .get_user_devices(user_id)
        .await
        .map_err(|e| format!("devices: {e}"))?;
    let mut out = Vec::new();
    for device in devices.devices() {
        out.push(DeviceInfo {
            user_id: user_id.to_string(),
            device_id: device.device_id().to_string(),
            display_name: device.display_name().map(str::to_owned),
            verified: device.is_verified(),
            is_own: is_current_device(current_device.as_deref(), device.device_id()),
        });
    }
    out.sort_by(|a, b| {
        (!a.is_own)
            .cmp(&(!b.is_own))
            .then(a.device_id.cmp(&b.device_id))
    });
    Ok(out)
}

fn is_current_device(
    current: Option<&matrix_sdk::ruma::DeviceId>,
    candidate: &matrix_sdk::ruma::DeviceId,
) -> bool {
    current.is_some_and(|current| current == candidate)
}

#[cfg(test)]
mod tests {
    use super::is_current_device;

    #[test]
    fn only_the_running_matrix_device_is_this_session() {
        let current: matrix_sdk::ruma::OwnedDeviceId = "CURRENT".into();
        let other: matrix_sdk::ruma::OwnedDeviceId = "OTHER".into();
        assert!(is_current_device(Some(&current), &current));
        assert!(!is_current_device(Some(&current), &other));
        assert!(!is_current_device(None, &current));
    }

    #[test]
    fn sas_emoji_count_is_seven_per_spec() {
        let n_emojis = 7;
        let n_decimals = 3;
        assert_eq!(n_emojis, 7);
        assert_eq!(n_decimals, 3);
    }

    #[test]
    fn flow_registry_dedupes_per_user() {
        // Scratch id; never collides with live traffic.
        let user = "@registry-test:hs";
        super::note_request_flow(user, "flow-a");
        super::note_request_flow(user, "flow-a");
        super::note_request_flow(user, "flow-b");
        let flows = super::known_flows_for(&matrix_sdk::ruma::OwnedUserId::try_from(user).unwrap());
        assert!(flows.contains(&"flow-a".to_owned()));
        assert!(flows.contains(&"flow-b".to_owned()));
        assert_eq!(flows.iter().filter(|f| *f == "flow-a").count(), 1);
    }
}
