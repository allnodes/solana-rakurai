use {
    borsh::BorshDeserialize,
    solana_account::ReadableAccount,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
};

/// Same program as virtual priority config; different PDA seed.
pub const POST_PACK_CONFIRMATION_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("L9D6MXnwqQeQhBJnJRXaNbqEnXu4WzubWRvmPhw4opt");

const POST_PACK_CONFIRMATION_CONFIG_SEED: &[u8] = b"POST_PACK_CONFIRMATION_ACCOUNT";
const ACCOUNT_HEADER_LEN: usize = 8 + 32 + 1 + 4;

#[derive(BorshDeserialize, Clone, Debug, PartialEq)]
struct PostPackConfirmationEntry {
    pub url: String,
    pub mask_signer: bool,
}

#[derive(BorshDeserialize, Clone, Debug, PartialEq)]
struct UuidPostPackConfirmationGroup {
    pub service_uuid: [u8; 32],
    pub entries: Vec<PostPackConfirmationEntry>,
}

#[derive(Clone, Debug)]
pub struct CachedUuidMevShareGroup {
    pub uuid: String,
    pub uuid_name: [u8; 32],
}

pub fn derive_post_pack_confirmation_config_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[POST_PACK_CONFIRMATION_CONFIG_SEED],
        &POST_PACK_CONFIRMATION_PROGRAM_ID,
    )
    .0
}

fn uuid_to_string(uuid: &[u8; 32]) -> String {
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

/// Loads unique service UUIDs from the on-chain post-pack confirmation config.
/// These UUIDs identify MEV-share collection accounts (MCAs).
pub fn load_cached_uuid_mev_share_groups(bank: &Bank) -> Option<Vec<CachedUuidMevShareGroup>> {
    let pda = derive_post_pack_confirmation_config_pda();
    let account = bank.get_account(&pda)?;
    if account.owner() != &POST_PACK_CONFIRMATION_PROGRAM_ID {
        return None;
    }
    let data = account.data();
    if data.len() <= ACCOUNT_HEADER_LEN {
        return None;
    }

    let mut slice = &data[ACCOUNT_HEADER_LEN..];
    let groups = Vec::<UuidPostPackConfirmationGroup>::deserialize(&mut slice).ok()?;
    Some(
        groups
            .into_iter()
            .filter(|group| group.service_uuid != [0u8; 32])
            .map(|group| CachedUuidMevShareGroup {
                uuid: uuid_to_string(&group.service_uuid),
                uuid_name: group.service_uuid,
            })
            .collect(),
    )
}
