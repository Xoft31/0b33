use crate::admin::require_admin_auth;
use crate::types::{DataKey, Dispute, DisputeStatus, Error, SubscriptionStatus};
use soroban_sdk::{contracttype, Address, Env, String, Symbol, Vec};

#[contracttype]
#[derive(Clone)]
pub struct BlocklistEntry {
    pub reason: String,
}

#[contracttype]
#[derive(Clone)]
pub struct BlocklistAddedEvent {
    pub subscriber: Address,
    pub reason: String,
    /// Event schema version for backwards-compatible indexer decoding.
    pub schema_version: u32,
    pub contract_id: Address,
}

#[contracttype]
#[derive(Clone)]
pub struct BlocklistRemovedEvent {
    pub subscriber: Address,
    /// Event schema version for backwards-compatible indexer decoding.
    pub schema_version: u32,
    pub contract_id: Address,
}

pub fn is_blocklisted(env: &Env, addr: &Address) -> bool {
    env.storage()
        .persistent()
        .has(&DataKey::Blocklist(addr.clone()))
}

pub fn require_not_blocklisted(env: &Env, addr: &Address) -> Result<(), Error> {
    if is_blocklisted(env, addr) {
        Err(Error::SubscriberBlocklisted)
    } else {
        Ok(())
    }
}

pub fn get_blocklist_entry(env: &Env, addr: Address) -> Result<BlocklistEntry, Error> {
    env.storage()
        .persistent()
        .get(&DataKey::Blocklist(addr))
        .ok_or(Error::NotFound)
}

pub fn do_add_to_blocklist(
    env: &Env,
    authorizer: Address,
    subscriber: Address,
    reason: Option<String>,
) -> Result<(), Error> {
    require_admin_auth(env, &authorizer)?;

    if is_blocklisted(env, &subscriber) {
        return Err(Error::SubscriberBlocklisted);
    }

    let reason_str = reason.unwrap_or_else(|| String::from_str(env, ""));
    let entry = BlocklistEntry {
        reason: reason_str.clone(),
    };
    env.storage()
        .persistent()
        .set(&DataKey::Blocklist(subscriber.clone()), &entry);

    env.events().publish(
        (Symbol::new(env, "blocklist_added"), subscriber.clone()),
        BlocklistAddedEvent {
            subscriber,
            reason: reason_str,
            schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(())
}

pub fn do_remove_from_blocklist(
    env: &Env,
    admin: Address,
    subscriber: Address,
) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;

    if !is_blocklisted(env, &subscriber) {
        return Err(Error::NotFound);
    }

    // Check for open disputes. A blocklisted subscriber with unresolved chargebacks
    // cannot be removed until all their disputes are resolved.
    let subs_key = DataKey::SubscriberSubs(subscriber.clone());
    if let Some(sub_ids) = env.storage().instance().get::<_, Vec<u32>>(&subs_key) {
        for sub_id in sub_ids.into_iter() {
            if let Some(dispute_id) = env
                .storage()
                .instance()
                .get::<_, u64>(&DataKey::SubscriptionDispute(sub_id))
            {
                if let Some(dispute) = env
                    .storage()
                    .persistent()
                    .get::<_, Dispute>(&DataKey::Dispute(dispute_id))
                {
                    if dispute.status == DisputeStatus::Open {
                        return Err(Error::SubscriberHasOpenDisputes);
                    }
                }
            }
        }
    }

    // Removing a blocklist entry restores the normal subscription lifecycle.
    // Capture the index before changing the blocklist, then resume only the
    // subscriptions that were explicitly paused (grace/insufficient states
    // must retain their billing semantics).
    let subscriber_subscriptions = env
        .storage()
        .instance()
        .get::<_, Vec<u32>>(&subs_key)
        .unwrap_or_else(|| Vec::new(env));

    env.storage()
        .persistent()
        .remove(&DataKey::Blocklist(subscriber.clone()));

    for sub_id in subscriber_subscriptions.iter() {
        if let Some(sub) = env
            .storage()
            .persistent()
            .get::<_, crate::types::Subscription>(&DataKey::Sub(sub_id))
        {
            if sub.status == SubscriptionStatus::Paused {
                crate::subscription::resume_after_blocklist_removal(
                    env,
                    sub_id,
                    admin.clone(),
                )?;
            }
        }
    }

    env.events().publish(
        (Symbol::new(env, "blocklist_removed"), subscriber.clone()),
        BlocklistRemovedEvent {
            subscriber,
            schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(())
}
