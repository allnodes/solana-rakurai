use {
    borsh::BorshDeserialize,
    solana_account::ReadableAccount,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::collections::HashMap,
};

pub const VIRTUAL_PRIORITY_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("L9D6MXnwqQeQhBJnJRXaNbqEnXu4WzubWRvmPhw4opt");

const VIRTUAL_PRIORITY_CONFIG_SEED: &[u8] = b"VIRTUAL_PRIORITY_CONFIG_ACCOUNT";
const ACCOUNT_HEADER_LEN: usize = 8 + 32 + 1 + 4;

#[derive(BorshDeserialize, Clone, Copy, Debug, PartialEq)]
struct PriorityEntry {
    key: Pubkey,
    value: f64,
}

#[derive(BorshDeserialize, Clone, Debug, PartialEq)]
struct UuidPriorityGroup {
    service_uuid: [u8; 32],
    entries: Vec<PriorityEntry>,
}

#[derive(Clone, Debug)]
pub struct CachedUuidTipGroup {
    pub uuid: String,
    pub uuid_name: [u8; 32],
    pub entries: Vec<(Pubkey, f64)>,
}

#[derive(Clone, Debug)]
pub struct TipUuidDelta {
    pub uuid: String,
    pub uuid_name: [u8; 32],
    pub amount: u64,
}

pub fn derive_virtual_priority_config_pda() -> Pubkey {
    Pubkey::find_program_address(&[VIRTUAL_PRIORITY_CONFIG_SEED], &VIRTUAL_PRIORITY_PROGRAM_ID).0
}

pub fn uuid_to_string(uuid: &[u8; 32]) -> String {
    let end = uuid
        .iter()
        .rposition(|b| *b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    if end == 0 {
        return String::new();
    }
    String::from_utf8_lossy(&uuid[..end]).into_owned()
}

pub fn load_cached_uuid_tip_groups(bank: &Bank) -> Option<Vec<CachedUuidTipGroup>> {
    let pda = derive_virtual_priority_config_pda();
    let account = bank.get_account(&pda)?;
    if account.owner() != &VIRTUAL_PRIORITY_PROGRAM_ID {
        return None;
    }
    let data = account.data();
    if data.len() <= ACCOUNT_HEADER_LEN {
        return None;
    }

    let mut slice = &data[ACCOUNT_HEADER_LEN..];
    let groups = Vec::<UuidPriorityGroup>::deserialize(&mut slice).ok()?;
    Some(
        groups
            .into_iter()
            .map(|group| CachedUuidTipGroup {
                uuid: uuid_to_string(&group.service_uuid),
                uuid_name: group.service_uuid,
                entries: group
                    .entries
                    .into_iter()
                    .map(|entry| (entry.key, entry.value))
                    .collect(),
            })
            .collect(),
    )
}

fn account_lamports(bank: &Bank, pubkey: &Pubkey) -> u64 {
    bank.get_account(pubkey)
        .map(|account| account.lamports())
        .unwrap_or(0)
}

fn weighted_lamport_delta(start_lamports: u64, end_lamports: u64, value: f64) -> u64 {
    let delta = end_lamports.saturating_sub(start_lamports);
    if value == 1.0 {
        delta
    } else {
        (delta as f64 * value) as u64
    }
}

/// Snapshots the current lamport balance of every tip account referenced by `groups`.
///
/// Balances must be captured while the source bank is still retained in `bank_forks`
/// (i.e. before the root advances past it and prunes it), because pruned banks can no
/// longer be read back.
pub fn snapshot_group_balances(
    bank: &Bank,
    groups: &[CachedUuidTipGroup],
) -> HashMap<Pubkey, u64> {
    let mut balances = HashMap::new();
    for group in groups {
        for (pubkey, _) in &group.entries {
            balances
                .entry(*pubkey)
                .or_insert_with(|| account_lamports(bank, pubkey));
        }
    }
    balances
}

/// Computes the weighted per-UUID tip delta from two previously captured balance snapshots.
pub fn weighted_tip_deltas_from_balances(
    start_balances: &HashMap<Pubkey, u64>,
    end_balances: &HashMap<Pubkey, u64>,
    groups: &[CachedUuidTipGroup],
) -> Vec<TipUuidDelta> {
    groups
        .iter()
        .map(|group| {
            let amount = group.entries.iter().fold(0u64, |acc, (pubkey, value)| {
                let start = start_balances.get(pubkey).copied().unwrap_or(0);
                let end = end_balances.get(pubkey).copied().unwrap_or(0);
                acc.saturating_add(weighted_lamport_delta(start, end, *value))
            });
            TipUuidDelta {
                uuid: group.uuid.clone(),
                uuid_name: group.uuid_name,
                amount,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uuid_to_string() {
        let mut uuid = [0u8; 32];
        uuid[..5].copy_from_slice(b"hello");
        assert_eq!(uuid_to_string(&uuid), "hello");
        assert_eq!(uuid_to_string(&[0u8; 32]), "");
    }

    #[test]
    fn test_weighted_lamport_delta() {
        assert_eq!(weighted_lamport_delta(100, 200, 1.0), 100);
        assert_eq!(weighted_lamport_delta(100, 200, 0.5), 50);
        assert_eq!(weighted_lamport_delta(200, 100, 1.0), 0);
    }
}
