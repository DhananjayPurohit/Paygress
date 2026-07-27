// Npub canonicalization and the checks built on it: warm-standby role
// assignment and offer author binding.

use super::wire::ProviderOfferContent;

/// Role this provider takes on a `WarmStandby` spawn request. `NotAddressed`
/// means the request must be rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmStandbyRole {
    Primary,
    Standby { index: usize, count: usize },
    NotAddressed,
}

pub fn warm_standby_role(
    self_npub: &str,
    primary_npub: &str,
    standby_providers: &[String],
) -> WarmStandbyRole {
    if npubs_equal(self_npub, primary_npub) {
        return WarmStandbyRole::Primary;
    }
    for (idx, p) in standby_providers.iter().enumerate() {
        if npubs_equal(self_npub, p) {
            return WarmStandbyRole::Standby {
                index: idx,
                count: standby_providers.len(),
            };
        }
    }
    WarmStandbyRole::NotAddressed
}

/// True iff two npub strings name the same key. Providers store their own npub
/// as hex while the consumer CLI ships bech32, so both sides must be
/// canonicalized before comparison — direct string comparison silently broke
/// warm-standby for every bech32 consumer.
///
/// Falls back to string equality only when *neither* side parses, which keeps
/// placeholder npubs in unit tests working without risking false positives on
/// real keys.
pub fn npubs_equal(a: &str, b: &str) -> bool {
    match (
        nostr_sdk::PublicKey::parse(a),
        nostr_sdk::PublicKey::parse(b),
    ) {
        (Ok(ka), Ok(kb)) => ka == kb,
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
        (Err(_), Err(_)) => a == b,
    }
}

/// True iff an offer's claimed `provider_npub` matches the key that signed the
/// event (`signer_hex` is `event.pubkey.to_hex()`).
///
/// nostr-sdk verifies the event signature on fetch, but `provider_npub` is a
/// free-form body field. Without this binding any key could publish an offer
/// impersonating any provider and last-write-wins over the real listing.
pub fn offer_authored_by_claimed_provider(signer_hex: &str, offer: &ProviderOfferContent) -> bool {
    npubs_equal(signer_hex, &offer.provider_npub)
}

#[cfg(test)]
mod offer_authenticity_tests {
    use super::*;
    use crate::nostr::{IsolationLevel, SCHEMA_VERSION};
    use nostr_sdk::{Keys, ToBech32};

    fn offer_claiming(npub: &str) -> ProviderOfferContent {
        ProviderOfferContent {
            provider_npub: npub.to_string(),
            hostname: "attacker-chosen.example".to_string(),
            location: None,
            capabilities: vec![],
            specs: vec![],
            whitelisted_mints: vec![],
            uptime_percent: 100.0,
            total_jobs_completed: 0,
            api_endpoint: None,
            version: SCHEMA_VERSION,
            isolation_level: IsolationLevel::SharedKernel,
            stake_proof: None,
        }
    }

    #[test]
    fn genuine_offer_from_its_signer_is_accepted() {
        let k = Keys::generate();
        let offer = offer_claiming(&k.public_key().to_hex());
        assert!(offer_authored_by_claimed_provider(
            &k.public_key().to_hex(),
            &offer
        ));
    }

    #[test]
    fn offer_claiming_foreign_npub_is_rejected() {
        let victim = Keys::generate();
        let attacker = Keys::generate();
        let forged = offer_claiming(&victim.public_key().to_hex());
        assert!(!offer_authored_by_claimed_provider(
            &attacker.public_key().to_hex(),
            &forged
        ));
    }

    #[test]
    fn signer_binding_canonicalizes_hex_and_bech32() {
        let k = Keys::generate();
        let offer = offer_claiming(&k.public_key().to_bech32().unwrap());
        assert!(offer_authored_by_claimed_provider(
            &k.public_key().to_hex(),
            &offer
        ));
    }
}

#[cfg(test)]
mod npubs_equal_tests {
    use super::*;

    // Two encodings of one frozen public key.
    const PUBKEY_BECH32: &str = "npub1ae40uj62de87f8tvx56e6ytp5m7jd7l96mh0ew43e8q5wucm7z9q2uqvuc";
    const PUBKEY_HEX: &str = "ee6afe4b4a6e4fe49d6c35359d1161a6fd26fbe5d6eefcbab1c9c147731bf08a";

    #[test]
    fn bech32_matches_itself() {
        assert!(npubs_equal(PUBKEY_BECH32, PUBKEY_BECH32));
    }

    #[test]
    fn hex_matches_itself() {
        assert!(npubs_equal(PUBKEY_HEX, PUBKEY_HEX));
    }

    /// Regression: the provider stores hex, the consumer ships bech32, and
    /// without normalization the role check always returned `NotAddressed`.
    #[test]
    fn bech32_matches_hex_for_same_key() {
        assert!(npubs_equal(PUBKEY_BECH32, PUBKEY_HEX));
        assert!(npubs_equal(PUBKEY_HEX, PUBKEY_BECH32));
    }

    #[test]
    fn different_keys_in_different_encodings_do_not_match() {
        let other_bech32 = "npub1hyr9m7zeegr98w4e07gvdpqrk25jfp3vku8029u8pcxsc48dq6nqxtwztv";
        assert!(!npubs_equal(PUBKEY_HEX, other_bech32));
    }

    #[test]
    fn unparseable_strings_fall_back_to_string_equality() {
        assert!(npubs_equal("npub1primary", "npub1primary"));
        assert!(!npubs_equal("npub1primary", "npub1secondary"));
    }

    #[test]
    fn one_real_one_typoed_returns_false() {
        assert!(!npubs_equal(PUBKEY_BECH32, "npub1primary"));
        assert!(!npubs_equal("npub1primary", PUBKEY_HEX));
    }
}
