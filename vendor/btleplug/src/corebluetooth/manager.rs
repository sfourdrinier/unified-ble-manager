// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

use super::adapter::Adapter;
use crate::{Result, api};
use async_trait::async_trait;

/// Implementation of [api::Manager](crate::api::Manager).
#[derive(Clone)]
pub struct Manager {
    authorities: std::sync::Arc<std::sync::Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    failure_evidence_store: Option<std::sync::Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Manager").finish_non_exhaustive()
    }
}

impl Manager {
    pub async fn new() -> Result<Self> {
        Ok(Self { authorities: std::sync::Arc::default(), failure_evidence_store: None })
    }

    pub async fn new_with_failure_evidence_store(
        failure_evidence_store: std::sync::Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>,
    ) -> Result<Self> {
        Ok(Self { authorities: std::sync::Arc::default(), failure_evidence_store: Some(failure_evidence_store) })
    }
}

#[async_trait]
impl api::Manager for Manager {
    type Adapter = Adapter;

    async fn adapters(&self) -> Result<Vec<Adapter>> {
        Ok(vec![Adapter::new_with_failure_evidence_store(
            self.authorities.clone(),
            self.failure_evidence_store.clone(),
        ).await?])
        // TODO What do we do if there is no bluetooth adapter, like on an older
        // macbook pro? Will BluetoothAdapter::init() fail?
    }
}
