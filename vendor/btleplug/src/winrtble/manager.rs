// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from Rumble
// (https://github.com/mwylde/rumble), using a dual MIT/Apache License under the
// following copyright:
//
// Copyright (c) 2014 The Rust Project Developers

use super::adapter::Adapter;
use crate::{Result, api};
use async_trait::async_trait;
use std::{
    future::IntoFuture,
    sync::{Arc, Mutex},
};
use windows::Devices::Radios::{Radio, RadioKind};

/// Implementation of [api::Manager](crate::api::Manager).
#[derive(Clone)]
pub struct Manager {
    reclaim_authorities: Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    peers: Arc<super::peripheral::PeerRegistry>,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager").finish_non_exhaustive()
    }
}

impl Manager {
    pub async fn new() -> Result<Self> {
        let (peers, reclaim_authorities) = super::peripheral::shared_peer_owners();
        Ok(Self {
            reclaim_authorities,
            peers,
        })
    }

    /// Construct a WinRT manager whose peers retain source-failure evidence in
    /// an explicit host-owned store. The default constructor intentionally
    /// does not choose a filesystem location; hosts that need bounded durable
    /// recovery provide the store and retain its lifecycle/retention policy.
    ///
    /// This manager owns an isolated peer registry so an unconfigured manager
    /// cannot accidentally reuse a configured peer (or vice versa).
    pub async fn new_with_failure_evidence_store(
        failure_evidence_store: Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>,
    ) -> Result<Self> {
        Ok(Self {
            reclaim_authorities: Arc::new(Mutex::new(Default::default())),
            peers: Arc::new(super::peripheral::PeerRegistry::new(Some(
                failure_evidence_store,
            ))),
        })
    }

    /// Whether this manager's real WinRT peripherals receive a configured
    /// durable failure-evidence store. This reports the instantiated manager,
    /// not a platform-wide guess, so hosts can expose an honest runtime
    /// capability when they choose the optional recovery storage.
    pub fn failure_evidence_configured(&self) -> bool {
        self.peers.failure_evidence_configured()
    }
}

#[async_trait]
impl api::Manager for Manager {
    type Adapter = Adapter;

    async fn adapters(&self) -> Result<Vec<Adapter>> {
        let radios = Radio::GetRadiosAsync()?.into_future().await?;
        radios
            .into_iter()
            .filter(|radio| radio.Kind() == Ok(RadioKind::Bluetooth))
            .map(|radio| {
                Adapter::from_radio_with_authorities(
                    radio,
                    self.reclaim_authorities.clone(),
                    self.peers.clone(),
                )
            })
            .collect()
    }
}
