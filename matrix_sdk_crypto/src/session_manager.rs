// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use dashmap::{DashMap, DashSet};
use matrix_sdk_common::{
    api::r0::{
        keys::claim_keys::{Request as KeysClaimRequest, Response as KeysClaimResponse},
        to_device::DeviceIdOrAllDevices,
    },
    assign,
    events::EventType,
    identifiers::{DeviceId, DeviceIdBox, DeviceKeyAlgorithm, UserId},
    uuid::Uuid,
};
use serde_json::{json, value::to_raw_value};
use tracing::{error, info, warn};

use crate::{
    error::OlmResult,
    key_request::KeyRequestMachine,
    olm::Account,
    requests::{OutgoingRequest, ToDeviceRequest},
    store::{Result as StoreResult, Store},
    Device,
};

#[derive(Debug, Clone)]
pub(crate) struct SessionManager {
    account: Account,
    store: Store,
    /// A map of user/devices that we need to automatically claim keys for.
    /// Submodules can insert user/device pairs into this map and the
    /// user/device paris will be added to the list of users when
    /// [`get_missing_sessions`](#method.get_missing_sessions) is called.
    users_for_key_claim: Arc<DashMap<UserId, DashSet<DeviceIdBox>>>,
    wedged_devices: Arc<DashMap<UserId, DashSet<DeviceIdBox>>>,
    key_request_machine: KeyRequestMachine,
    outgoing_to_device_requests: Arc<DashMap<Uuid, OutgoingRequest>>,
}

impl SessionManager {
    const KEY_CLAIM_TIMEOUT: Duration = Duration::from_secs(10);
    const UNWEDGING_INTERVAL: Duration = Duration::from_secs(60 * 60);

    pub fn new(
        account: Account,
        users_for_key_claim: Arc<DashMap<UserId, DashSet<DeviceIdBox>>>,
        key_request_machine: KeyRequestMachine,
        store: Store,
    ) -> Self {
        Self {
            account,
            store,
            key_request_machine,
            users_for_key_claim,
            wedged_devices: Arc::new(DashMap::new()),
            outgoing_to_device_requests: Arc::new(DashMap::new()),
        }
    }

    /// Mark the outgoing request as sent.
    pub fn mark_outgoing_request_as_sent(&self, id: &Uuid) {
        self.outgoing_to_device_requests.remove(id);
    }

    pub async fn mark_device_as_wedged(&self, sender: &UserId, curve_key: &str) -> StoreResult<()> {
        if let Some(device) = self
            .store
            .get_device_from_curve_key(sender, curve_key)
            .await?
        {
            let sessions = device.get_sessions().await?;

            if let Some(sessions) = sessions {
                let mut sessions = sessions.lock().await;
                sessions.sort_by_key(|s| s.creation_time.clone());

                let session = sessions.get(0);

                if let Some(session) = session {
                    if session.creation_time.elapsed() > Self::UNWEDGING_INTERVAL {
                        self.wedged_devices
                            .entry(device.user_id().to_owned())
                            .or_insert_with(DashSet::new)
                            .insert(device.device_id().into());
                    }
                }
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn is_device_wedged(&self, device: &Device) -> bool {
        self.wedged_devices
            .get(device.user_id())
            .map(|d| d.contains(device.device_id()))
            .unwrap_or(false)
    }

    /// Check if the session was created to unwedge a Device.
    ///
    /// If the device was wedged this will queue up a dummy to-device message.
    async fn check_if_unwedged(&self, user_id: &UserId, device_id: &DeviceId) -> OlmResult<()> {
        if self
            .wedged_devices
            .get(user_id)
            .map(|d| d.remove(device_id))
            .flatten()
            .is_some()
        {
            if let Some(device) = self.store.get_device(user_id, device_id).await? {
                let content = device.encrypt(EventType::Dummy, json!({})).await?;
                let id = Uuid::new_v4();
                let mut messages = BTreeMap::new();

                messages
                    .entry(device.user_id().to_owned())
                    .or_insert_with(BTreeMap::new)
                    .insert(
                        DeviceIdOrAllDevices::DeviceId(device.device_id().into()),
                        to_raw_value(&content)?,
                    );

                let request = OutgoingRequest {
                    request_id: id,
                    request: Arc::new(
                        ToDeviceRequest {
                            event_type: EventType::RoomEncrypted,
                            txn_id: id,
                            messages,
                        }
                        .into(),
                    ),
                };

                self.outgoing_to_device_requests.insert(id, request);
            }
        }

        Ok(())
    }

    /// Get the a key claiming request for the user/device pairs that we are
    /// missing Olm sessions for.
    ///
    /// Returns None if no key claiming request needs to be sent out.
    ///
    /// Sessions need to be established between devices so group sessions for a
    /// room can be shared with them.
    ///
    /// This should be called every time a group session needs to be shared as
    /// well as between sync calls. After a sync some devices may request room
    /// keys without us having a valid Olm session with them, making it
    /// impossible to server the room key request, thus it's necessary to check
    /// for missing sessions between sync as well.
    ///
    /// **Note**: Care should be taken that only one such request at a time is
    /// in flight, e.g. using a lock.
    ///
    /// The response of a successful key claiming requests needs to be passed to
    /// the `OlmMachine` with the [`receive_keys_claim_response`].
    ///
    /// # Arguments
    ///
    /// `users` - The list of users that we should check if we lack a session
    /// with one of their devices. This can be an empty iterator when calling
    /// this method between sync requests.
    ///
    /// [`receive_keys_claim_response`]: #method.receive_keys_claim_response
    pub async fn get_missing_sessions(
        &self,
        users: &mut impl Iterator<Item = &UserId>,
    ) -> OlmResult<Option<(Uuid, KeysClaimRequest)>> {
        let mut missing = BTreeMap::new();

        // Add the list of devices that the user wishes to establish sessions
        // right now.
        for user_id in users {
            let user_devices = self.store.get_user_devices(user_id).await?;

            for device in user_devices.devices() {
                let sender_key = if let Some(k) = device.get_key(DeviceKeyAlgorithm::Curve25519) {
                    k
                } else {
                    continue;
                };

                let sessions = self.store.get_sessions(sender_key).await?;

                let is_missing = if let Some(sessions) = sessions {
                    sessions.lock().await.is_empty()
                } else {
                    true
                };

                if is_missing {
                    missing
                        .entry(user_id.to_owned())
                        .or_insert_with(BTreeMap::new)
                        .insert(
                            device.device_id().into(),
                            DeviceKeyAlgorithm::SignedCurve25519,
                        );
                }
            }
        }

        // Add the list of sessions that for some reason automatically need to
        // create an Olm session.
        for item in self.users_for_key_claim.iter() {
            let user = item.key();

            for device_id in item.value().iter() {
                missing
                    .entry(user.to_owned())
                    .or_insert_with(BTreeMap::new)
                    .insert(device_id.to_owned(), DeviceKeyAlgorithm::SignedCurve25519);
            }
        }

        if missing.is_empty() {
            Ok(None)
        } else {
            Ok(Some((
                Uuid::new_v4(),
                assign!(KeysClaimRequest::new(missing), {
                    timeout: Some(Self::KEY_CLAIM_TIMEOUT),
                }),
            )))
        }
    }

    /// Receive a successful key claim response and create new Olm sessions with
    /// the claimed keys.
    ///
    /// # Arguments
    ///
    /// * `response` - The response containing the claimed one-time keys.
    pub async fn receive_keys_claim_response(&self, response: &KeysClaimResponse) -> OlmResult<()> {
        // TODO log the failures here

        for (user_id, user_devices) in &response.one_time_keys {
            for (device_id, key_map) in user_devices {
                let device = match self.store.get_readonly_device(&user_id, device_id).await {
                    Ok(Some(d)) => d,
                    Ok(None) => {
                        warn!(
                            "Tried to create an Olm session for {} {}, but the device is unknown",
                            user_id, device_id
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            "Tried to create an Olm session for {} {}, but \
                            can't fetch the device from the store {:?}",
                            user_id, device_id, e
                        );
                        continue;
                    }
                };

                info!("Creating outbound Session for {} {}", user_id, device_id);

                let session = match self.account.create_outbound_session(device, &key_map).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("{:?}", e);
                        continue;
                    }
                };

                if let Err(e) = self.store.save_sessions(&[session]).await {
                    error!("Failed to store newly created Olm session {}", e);
                    continue;
                }

                self.key_request_machine.retry_keyshare(&user_id, device_id);

                if let Err(e) = self.check_if_unwedged(&user_id, device_id).await {
                    error!(
                        "Error while treating an unwedged device {} {} {:?}",
                        user_id, device_id, e
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use dashmap::DashMap;
    use std::{collections::BTreeMap, sync::Arc};

    use matrix_sdk_common::{
        api::r0::keys::claim_keys::Response as KeyClaimResponse,
        identifiers::{user_id, DeviceIdBox, UserId},
    };
    use matrix_sdk_test::async_test;

    use super::SessionManager;
    use crate::{
        identities::ReadOnlyDevice,
        key_request::KeyRequestMachine,
        olm::{Account, ReadOnlyAccount},
        store::{CryptoStore, MemoryStore, Store},
        verification::VerificationMachine,
    };

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn device_id() -> DeviceIdBox {
        "DEVICEID".into()
    }

    fn bob_account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(&user_id!("@bob:localhost"), "BOBDEVICE".into())
    }

    async fn session_manager() -> SessionManager {
        let user_id = user_id();
        let device_id = device_id();

        let outbound_sessions = Arc::new(DashMap::new());
        let users_for_key_claim = Arc::new(DashMap::new());
        let account = ReadOnlyAccount::new(&user_id, &device_id);
        let store: Arc<Box<dyn CryptoStore>> = Arc::new(Box::new(MemoryStore::new()));
        store.save_account(account.clone()).await.unwrap();

        let verification = VerificationMachine::new(account.clone(), store.clone());

        let user_id = Arc::new(user_id);
        let device_id = Arc::new(device_id);

        let store = Store::new(user_id.clone(), store, verification);

        let account = Account {
            inner: account,
            store: store.clone(),
        };

        let key_request = KeyRequestMachine::new(
            user_id,
            device_id,
            store.clone(),
            outbound_sessions,
            users_for_key_claim.clone(),
        );

        SessionManager::new(account, users_for_key_claim, key_request, store)
    }

    #[async_test]
    async fn session_creation() {
        let manager = session_manager().await;
        let bob = bob_account();

        let bob_device = ReadOnlyDevice::from_account(&bob).await;

        manager.store.save_devices(&[bob_device]).await.unwrap();

        let (_, request) = manager
            .get_missing_sessions(&mut [bob.user_id().clone()].iter())
            .await
            .unwrap()
            .unwrap();

        assert!(request.one_time_keys.contains_key(bob.user_id()));

        bob.generate_one_time_keys_helper(1).await;
        let one_time = bob.signed_one_time_keys_helper().await.unwrap();
        bob.mark_keys_as_published().await;

        let mut one_time_keys = BTreeMap::new();
        one_time_keys
            .entry(bob.user_id().clone())
            .or_insert_with(BTreeMap::new)
            .insert(bob.device_id().into(), one_time);

        let response = KeyClaimResponse {
            failures: BTreeMap::new(),
            one_time_keys,
        };

        manager
            .receive_keys_claim_response(&response)
            .await
            .unwrap();

        assert!(manager
            .get_missing_sessions(&mut [bob.user_id().clone()].iter())
            .await
            .unwrap()
            .is_none());
    }
}
